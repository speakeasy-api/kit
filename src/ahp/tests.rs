use std::sync::{Arc, Mutex};

use agenthooksprotocol::SessionStartInputTrigger;
use agentkit_core::{
    DataRef, FinishReason, Item, ItemKind, MetadataMap, Modality, Part, SessionId, ToolCallId,
    ToolOutput, ToolResultPart, TurnCancellation, TurnId,
};
use agentkit_loop::{
    Agent, LoopError, LoopStep, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent,
    ModelTurnResult, SessionConfig, TurnRequest,
};
use agentkit_tools_core::{
    AllowAllPermissions, OwnedToolContext, Tool, ToolContext, ToolError, ToolExecutionOutcome,
    ToolName, ToolRegistry, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::test_support::{hooks, intercept, observe, text_message};
use super::*;

#[cfg(unix)]
#[tokio::test]
async fn stdio_hooks_inherit_environment_with_explicit_overrides() {
    const VARIABLE: &str = "KIT_AHP_TEST_INHERITED_ENV";
    const VALUE: &str = "environment-sentinel";
    // Configure only an isolated test process; other tests spawn hooks too.
    if std::env::var(VARIABLE).ok().as_deref() != Some(VALUE) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ahp::tests::stdio_hooks_inherit_environment_with_explicit_overrides",
                "--nocapture",
            ])
            .env(VARIABLE, VALUE)
            .env("HERDR_ENV", "1")
            .env("HERDR_BIN_PATH", "herdr-bin-sentinel")
            .env("HERDR_PANE_ID", "host-pane")
            .env("HERDR_SOCKET_PATH", "socket-sentinel")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("hook-env");
    let registration = serde_json::from_value(json!({
        "protocolVersion": "draft",
        "hooks": [{
            "id": "dev.kit.environment-test",
            "transport": {
                "type": "stdio",
                "command": "/bin/sh",
                "args": [
                    "-c",
                    "read -r event; printf '%s\\n' \"$HERDR_ENV\" \"$HERDR_BIN_PATH\" \"$HERDR_PANE_ID\" \"$HERDR_SOCKET_PATH\" \"$KIT_AHP_TEST_INHERITED_ENV\" > \"$1\"",
                    "environment-hook",
                    output,
                ],
                "env": {"HERDR_PANE_ID": "configured-pane"},
                "lifecycle": "per_event",
            },
            "subscriptions": [observe(&["session.start"])],
        }],
    }))
    .unwrap();
    let hooks = Hooks::new(registration, options().unwrap()).unwrap();
    hooks
        .session_start(
            "environment-test",
            SessionStartInputTrigger::Startup,
            None,
            false,
        )
        .await
        .unwrap();
    hooks.sdk.shutdown().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(output).unwrap(),
        "1\nherdr-bin-sentinel\nconfigured-pane\nsocket-sentinel\nenvironment-sentinel\n",
    );
}

#[tokio::test]
async fn registration_headers_reach_http_hook_requests() {
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
    use tokio::sync::mpsc;

    let (sender, mut received) = mpsc::channel(1);
    let app = Router::new()
        .route(
            "/hooks",
            post(
                |State(sender): State<mpsc::Sender<HeaderMap>>,
                 headers: HeaderMap,
                 Json(request): Json<Value>| async move {
                    sender.send(headers).await.unwrap();
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {"protocolVersion": "draft", "effects": []},
                    }))
                },
            ),
        )
        .with_state(sender);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/hooks", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(CONFIG_FILE);
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "protocolVersion": "draft",
            "hooks": [{
                "id": "dev.kit.headers",
                "transport": {
                    "type": "http",
                    "url": endpoint,
                    "headers": {
                        "Authorization": "Bearer test-only-header",
                        "X-Kit-Test": "configured-header",
                    },
                },
                "subscriptions": [intercept(&["user.message.inbound"])],
            }],
        }))
        .unwrap(),
    )
    .unwrap();
    let mut options = options().unwrap();
    options.backend.allow_loopback_http = true;
    let hooks = Arc::new(Hooks::new(read(&path).unwrap().unwrap(), options).unwrap());
    prompt(&hooks, &Model::default(), "hello").await;
    let headers = tokio::time::timeout(std::time::Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    server.abort();
    assert_eq!(headers["authorization"], "Bearer test-only-header");
    assert_eq!(headers["x-kit-test"], "configured-header");
}

#[test]
fn every_intercept_event_accepts_observe_subscriptions() {
    for event in [
        "user.message.inbound",
        "model.request.before",
        "tool.before",
        "tool.after",
        "context.compact.before",
        "context.compact.after",
    ] {
        hooks(vec![intercept(&[event])]);
        hooks(vec![observe(&[event])]);
    }
}

#[test]
fn every_granted_event_accepts_wildcard_subscriptions() {
    hooks(vec![intercept(&["*"]), observe(&["*"])]);
}

#[tokio::test]
async fn every_lifecycle_event_reports_harness_identity() {
    let (hooks, backend) = hooks(vec![intercept(&["*"]), observe(&["*"])]);
    prompt(&hooks, &Model::default(), "hello").await;
    call(&hooks, &Echo::new(), json!({"text": "hello"})).await;
    hooks
        .session_end("abc", SessionEndInputOutcome::Completed)
        .await;
    let items = [Item::text(ItemKind::User, "old")];
    hooks
        .compact_before("abc", &items, true, "instructions", None)
        .await;
    hooks
        .compact_after("abc", &items, "summary".into(), false)
        .await;

    let input = agenthooksprotocol::inputs::ModelErrorInput::new(
        Box::new(agenthooksprotocol::ExecutionEventAttempt::new(
            "attempt",
            agenthooksprotocol::Integer::new(1.into()).unwrap(),
        )),
        Box::new(agenthooksprotocol::ExecutionEventError::new(
            "provider", "failed",
        )),
        agenthooksprotocol::ModelErrorInputExecution::Executed(
            agenthooksprotocol::ModelErrorInputExecutionExecuted::new(),
        ),
        Box::new(agenthooksprotocol::ExecutionEventModel::new("test", "test")),
    );
    hooks.sdk.model_error(input).await.unwrap();

    for event in [
        "session.start",
        "session.end",
        "turn.start",
        "turn.end",
        "user.message.inbound",
        "model.request.before",
        "model.response.after",
        "model.error",
        "tool.before",
        "tool.after",
        "context.compact.before",
        "context.compact.after",
    ] {
        let delivered = backend.events(event);
        assert!(!delivered.is_empty(), "missing {event}");
        for delivered in delivered {
            assert!(delivered["id"].is_string(), "{event}");
            assert!(delivered["time"].is_string(), "{event}");
            assert_eq!(delivered["source"], SOURCE);
            assert_eq!(
                delivered["harness"],
                json!({
                    "name": "kit", "version": env!("CARGO_PKG_VERSION"),
                }),
                "{event}"
            );
        }
    }
}

#[test]
fn project_hooks_replace_global_hooks_with_the_same_id() {
    let registration = |hooks: Value| -> Registration {
        serde_json::from_value(json!({"protocolVersion": "draft", "hooks": hooks})).unwrap()
    };
    let backend = |id: &str, command: &str| {
        json!({
            "id": id,
            "transport": {"type": "stdio", "command": command, "lifecycle": "persistent"},
            "subscriptions": [observe(&["session.start"])],
        })
    };
    let mut merged = None;
    merge(
        &mut merged,
        registration(json!([
            backend("dev.kit.a", "global-a"),
            backend("dev.kit.b", "global-b")
        ])),
    );
    merge(
        &mut merged,
        registration(json!([
            backend("dev.kit.b", "project-b"),
            backend("dev.kit.c", "project-c")
        ])),
    );

    let merged = serde_json::to_value(merged.unwrap()).unwrap();
    let commands: Vec<_> = merged["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hook| hook["transport"]["command"].as_str().unwrap())
        .collect();
    assert_eq!(commands, ["global-a", "project-b", "project-c"]);
}

#[test]
fn unreadable_registration_is_reported_and_missing_is_ignored() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(CONFIG_FILE);
    assert!(read(&path).unwrap().is_none());

    crate::resilient_fs::write(&path, b"{\"hooks\": 1}").unwrap();
    assert!(read(&path).is_err());
}

struct Echo {
    spec: ToolSpec,
    current: Mutex<Option<ToolSpec>>,
    received: Mutex<Vec<Value>>,
}

impl Echo {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            spec: ToolSpec::new(
                ToolName::new("echo"),
                "echo",
                json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                    "additionalProperties": false,
                }),
            ),
            current: Mutex::default(),
            received: Mutex::default(),
        })
    }
}

#[async_trait]
impl Tool for Echo {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn current_spec(&self) -> Option<ToolSpec> {
        self.current.lock().unwrap().clone()
    }

    async fn invoke_outcome(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        if request.input["text"] == "unavailable" {
            return ToolExecutionOutcome::FailedBeforeInvocation(ToolError::Unavailable(
                "offline".into(),
            ));
        }
        if request.input["text"] == "cancelled" {
            return ToolExecutionOutcome::Failed(ToolError::Cancelled);
        }
        match self.invoke(request, context).await {
            Ok(result) => ToolExecutionOutcome::Completed(result),
            Err(error) => ToolExecutionOutcome::Failed(error),
        }
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        tokio::task::yield_now().await;
        self.received.lock().unwrap().push(request.input.clone());
        let text = request.input["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            if text == "oversized-binary" {
                oversized_binary_output()
            } else {
                ToolOutput::text(text)
            },
        ))
        .with_duration(std::time::Duration::from_millis(7)))
    }
}

async fn call(hooks: &Arc<Hooks>, echo: &Arc<Echo>, input: Value) -> ToolExecutionOutcome {
    let mut registry = ToolRegistry::new();
    registry.register_arc(echo.clone());
    let tool = hooked_tools(registry, hooks)
        .get(&ToolName::new("echo"))
        .unwrap();
    let context = OwnedToolContext {
        session_id: SessionId::new("session"),
        turn_id: TurnId::new("turn"),
        metadata: MetadataMap::new(),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: None,
        execution_scope: None,
        approved_request: None,
    };
    let request = ToolRequest::new(
        ToolCallId::new("call"),
        ToolName::new("echo"),
        input,
        context.session_id.clone(),
        context.turn_id.clone(),
    );
    tool.invoke_outcome(request, &mut context.borrowed()).await
}

fn output(outcome: ToolExecutionOutcome) -> ToolOutput {
    match outcome {
        ToolExecutionOutcome::Completed(result) => result.result.output,
        other => panic!("expected a completed call, got {other:?}"),
    }
}

fn denial_message(outcome: ToolExecutionOutcome) -> String {
    match outcome {
        ToolExecutionOutcome::FailedBeforeInvocation(ToolError::PermissionDenied(denial)) => {
            denial.message
        }
        other => panic!("expected a denied call, got {other:?}"),
    }
}

#[tokio::test]
async fn tool_before_denial_skips_the_tool() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.before"])]);
    backend.respond(
        "tool.before",
        json!([{"type": "deny", "reason": "not here"}]),
    );
    let echo = Echo::new();

    let outcome = call(&hooks, &echo, json!({"text": "hello"})).await;

    assert_eq!(denial_message(outcome), "not here");
    assert!(echo.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn tool_before_rejects_malformed_original_arguments_without_dispatch() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.before", "tool.after"])]);
    let mut echo = Echo::new();
    // Use a current schema distinct from the advertised schema, as dynamic tools do.
    *Arc::get_mut(&mut echo).unwrap().current.get_mut().unwrap() = Some(ToolSpec::new(
        ToolName::new("echo"),
        "echo",
        json!({
            "type": "object",
            "properties": {"offset": {"type": "integer", "minimum": 0}},
            "required": ["offset"],
        }),
    ));

    let outcome = call(&hooks, &echo, json!({"offset": "0"})).await;

    match outcome {
        ToolExecutionOutcome::FailedBeforeInvocation(ToolError::InvalidInput(message)) => {
            assert!(message.contains("echo"), "{message}");
            assert!(message.contains("/offset"), "{message}");
            assert!(message.contains("integer"), "{message}");
        }
        other => panic!("expected invalid input, got {other:?}"),
    }
    assert!(echo.received.lock().unwrap().is_empty());
    assert!(backend.all_events().is_empty());

    output(call(&hooks, &echo, json!({"offset": 0})).await);
    assert_eq!(*echo.received.lock().unwrap(), [json!({"offset": 0})]);
    assert_eq!(backend.events("tool.before").len(), 1);
    assert_eq!(backend.events("tool.after").len(), 1);
}

#[tokio::test]
async fn tool_before_reports_invalid_schema_as_internal_without_dispatch() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.before", "tool.after"])]);
    let mut echo = Echo::new();
    Arc::get_mut(&mut echo).unwrap().spec.input_schema = json!({"type": "not-a-type"});

    let outcome = call(&hooks, &echo, json!({"text": "hello"})).await;

    match outcome {
        ToolExecutionOutcome::FailedBeforeInvocation(ToolError::Internal(message)) => {
            assert!(message.contains("echo"), "{message}");
            assert!(message.contains("invalid tool schema"), "{message}");
        }
        other => panic!("expected internal error, got {other:?}"),
    }
    assert!(echo.received.lock().unwrap().is_empty());
    assert!(backend.all_events().is_empty());
}

#[tokio::test]
async fn tool_before_input_edits_reach_the_tool() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.before"])]);
    backend.respond(
        "tool.before",
        json!([{"type": "modify", "target": "input", "operation": "merge", "value": {"text": "edited"}}]),
    );
    let echo = Echo::new();

    let outcome = call(&hooks, &echo, json!({"text": "hello"})).await;

    assert_eq!(output(outcome), ToolOutput::text("edited"));
    assert_eq!(*echo.received.lock().unwrap(), [json!({"text": "edited"})]);
}

#[tokio::test]
async fn tool_before_edits_must_satisfy_the_tool_schema() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.before"])]);
    backend.respond(
        "tool.before",
        json!([{"type": "modify", "target": "input", "operation": "merge", "value": {"extra": true}}]),
    );
    let echo = Echo::new();

    let outcome = call(&hooks, &echo, json!({"text": "hello"})).await;

    denial_message(outcome);
    assert!(echo.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn tool_before_validates_against_the_current_schema() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.before"])]);
    backend.respond(
        "tool.before",
        json!([{"type": "modify", "target": "input", "operation": "merge", "value": {"extra": true}}]),
    );
    let echo = Echo::new();
    denial_message(call(&hooks, &echo, json!({"text": "hello"})).await);

    *echo.current.lock().unwrap() = Some(ToolSpec::new(
        ToolName::new("echo"),
        "echo",
        json!({"type": "object"}),
    ));
    call(&hooks, &echo, json!({"text": "hello"})).await;

    assert_eq!(
        *echo.received.lock().unwrap(),
        [json!({"text": "hello", "extra": true})]
    );
}

#[tokio::test]
async fn unsubscribed_events_leave_tools_and_prompts_unchanged() {
    let (hooks, _) = hooks(vec![observe(&["session.start"])]);
    let echo = Echo::new();
    let model = Model::default();

    let outcome = call(&hooks, &echo, json!({"text": "hello"})).await;
    let (_, persisted) = prompt(&hooks, &model, "original").await;

    assert_eq!(output(outcome), ToolOutput::text("hello"));
    let requests = model.requests.lock().unwrap();
    assert_eq!(last_user_text(&requests[0]).as_deref(), Some("original"));
    assert_eq!(last_user_text(&persisted).as_deref(), Some("original"));
}

#[tokio::test]
async fn tool_before_return_supplies_the_result() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.before"])]);
    backend.respond(
        "tool.before",
        json!([{"type": "return", "value": "cached"}]),
    );
    let echo = Echo::new();

    let outcome = call(&hooks, &echo, json!({"text": "hello"})).await;

    assert_eq!(output(outcome), ToolOutput::text("cached"));
    assert!(echo.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn tool_before_edited_return_requires_explicit_hook_allow() {
    for allowed in [false, true] {
        let (hooks, backend) = hooks(vec![intercept(&["tool.before"])]);
        let mut effects = vec![
            json!({"type": "modify", "target": "input", "operation": "merge", "value": {"text": "edited"}}),
            json!({"type": "return", "value": "cached"}),
        ];
        if allowed {
            effects.push(json!({"type": "allow"}));
        }
        backend.respond("tool.before", json!(effects));
        let outcome = call(&hooks, &Echo::new(), json!({"text": "hello"})).await;

        assert_eq!(
            output(outcome),
            ToolOutput::text(if allowed { "cached" } else { "edited" }),
        );
    }
}

fn output_replacement(text: &str) -> Value {
    json!({"type": "modify", "target": "output", "operation": "replace", "value": [
        {"id": "replacement", "role": "tool", "parts": [
            {"id": "replacement/text", "kind": "text", "mediaType": "text/plain",
             "selection": "body", "text": text}
        ]}
    ]})
}

#[tokio::test]
async fn tool_after_replaces_output_and_preserves_execution_metadata() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.after"])]);
    backend.respond("tool.after", json!([output_replacement("redacted")]));
    let echo = Echo::new();
    let outcome = call(&hooks, &echo, json!({"text": "secret"})).await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("expected completed call")
    };
    assert_eq!(result.result.output, ToolOutput::text("redacted"));
    assert_eq!(result.result.call_id.to_string(), "call");
    assert!(!result.result.is_error);
    assert_eq!(result.duration, Some(std::time::Duration::from_millis(7)));
    assert_eq!(
        echo.received.lock().unwrap().as_slice(),
        &[json!({"text": "secret"})]
    );
}

#[tokio::test]
async fn tool_after_stop_blocks_output_even_after_replacement() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.after"])]);
    backend.respond(
        "tool.after",
        json!([
            output_replacement("redacted"),
            {"type": "flow", "operation": "stop", "reason": "sensitive output"}
        ]),
    );
    let echo = Echo::new();
    let outcome = call(&hooks, &echo, json!({"text": "secret"})).await;
    assert!(matches!(
        outcome,
        ToolExecutionOutcome::Failed(ToolError::PermissionDenied(_))
    ));
    assert_eq!(
        echo.received.lock().unwrap().as_slice(),
        &[json!({"text": "secret"})]
    );
}

#[tokio::test]
async fn tool_after_replaces_supplied_results_without_running_tool() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.before", "tool.after"])]);
    backend.respond(
        "tool.before",
        json!([{"type": "return", "value": "cached"}]),
    );
    backend.respond("tool.after", json!([output_replacement("redacted")]));
    let echo = Echo::new();
    assert_eq!(
        output(call(&hooks, &echo, json!({"text": "secret"})).await),
        ToolOutput::text("redacted")
    );
    assert!(echo.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn tool_after_replacement_does_not_turn_failure_into_success() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.after"])]);
    backend.respond("tool.after", json!([output_replacement("success")]));
    assert!(matches!(
        call(&hooks, &Echo::new(), json!({"text": "unavailable"})).await,
        ToolExecutionOutcome::FailedBeforeInvocation(ToolError::Unavailable(_))
    ));
}

#[tokio::test]
async fn tool_after_empty_replacement_removes_output() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.after"])]);
    backend.respond(
        "tool.after",
        json!([{"type":"modify", "target":"output", "operation":"replace", "value":[]}]),
    );
    assert_eq!(
        output(call(&hooks, &Echo::new(), json!({"text": "secret"})).await),
        ToolOutput::text("")
    );
}

#[test]
fn only_response_interceptors_require_buffering() {
    let (response_hooks, _) = hooks(vec![intercept(&["model.response.after"])]);
    assert!(response_hooks.intercepts_model_response);
    for subscriptions in [
        vec![observe(&["model.response.after"])],
        vec![intercept(&["tool.before"])],
        vec![intercept(&["model.request.before"])],
    ] {
        let (hooks, _) = hooks(subscriptions);
        assert!(!hooks.intercepts_model_response);
    }
}

#[tokio::test]
async fn hook_messages_reach_the_owning_session_without_entering_tool_output() {
    let (sender, receiver) = std::sync::mpsc::channel();
    let (hooks, backend) = hooks(vec![intercept(&["tool.before", "tool.after"])]);
    let _subscription = hooks.subscribe_messages("session".into(), move |text| {
        sender.send(text.to_owned()).is_ok()
    });
    backend.respond("tool.before", json!([{"type":"message", "text":"before"}]));
    backend.respond("tool.after", json!([{"type":"message", "text":"after"}]));
    assert_eq!(
        output(call(&hooks, &Echo::new(), json!({"text":"result"})).await),
        ToolOutput::text("result")
    );
    assert_eq!(receiver.try_iter().collect::<Vec<_>>(), ["before", "after"]);
}

fn context_injection(text: &str) -> Value {
    json!({"type":"inject", "target":"context", "operation":"append", "deliverAt":"now",
    "value":[{"id":"context", "role":"user", "parts":[
        {"id":"context/text", "kind":"text", "mediaType":"text/plain", "selection":"body", "text":text}
    ]}]})
}

#[tokio::test]
async fn session_and_tool_context_wait_for_the_next_request() {
    let (hooks, backend) = hooks(vec![intercept(&["session.start", "tool.after"])]);
    backend.respond(
        "session.start",
        json!([context_injection("session context")]),
    );
    backend.respond("tool.after", json!([context_injection("tool context")]));
    let _lease = hooks
        .session_start("session", SessionStartInputTrigger::Startup, None, false)
        .await
        .unwrap();
    call(&hooks, &Echo::new(), json!({"text":"result"})).await;
    let delivery = hooks.injections.claim("session").unwrap();
    let mut items = Vec::new();
    delivery.append_to(&mut items);
    assert_eq!(items.len(), 2);
    assert_eq!(
        super::part_text(&items[0].parts[0]).as_deref(),
        Some("session context")
    );
    assert_eq!(
        super::part_text(&items[1].parts[0]).as_deref(),
        Some("tool context")
    );
    delivery.commit().unwrap();
    hooks
        .session_end("session", SessionEndInputOutcome::Completed)
        .await;
    assert!(hooks.injections.claim("session").is_err());
}

#[tokio::test]
async fn failed_tools_can_inject_recovery_context_without_becoming_successful() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.after"])]);
    let _lease = hooks
        .session_start("session", SessionStartInputTrigger::Startup, None, false)
        .await
        .unwrap();
    backend.respond("tool.after", json!([context_injection("recovery context")]));
    assert!(matches!(
        call(&hooks, &Echo::new(), json!({"text":"unavailable"})).await,
        ToolExecutionOutcome::FailedBeforeInvocation(ToolError::Unavailable(_))
    ));
    let delivery = hooks.injections.claim("session").unwrap();
    let mut items = Vec::new();
    delivery.append_to(&mut items);
    assert_eq!(items.len(), 1);
    assert_eq!(
        super::part_text(&items[0].parts[0]).as_deref(),
        Some("recovery context")
    );
}

fn oversized_binary_output() -> ToolOutput {
    ToolOutput::Parts(vec![Part::Media(agentkit_core::MediaPart::new(
        Modality::Image,
        "image/png",
        DataRef::inline_bytes(vec![42; 4 * 1024 * 1024 + 1]),
    ))])
}

#[tokio::test]
async fn tool_after_observer_preparation_failure_preserves_native_binary_output() {
    let (hooks, backend) = hooks(vec![observe(&["tool.after"])]);
    let (sender, receiver) = std::sync::mpsc::channel();
    let _subscription = hooks.subscribe_messages("session".into(), move |text| {
        sender.send(text.to_owned()).is_ok()
    });
    let echo = Echo::new();
    let outcome = call(&hooks, &echo, json!({"text": "oversized-binary"})).await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("observer preparation failure replaced the native result");
    };
    assert_eq!(result.result.output, oversized_binary_output());
    assert_eq!(result.result.call_id, ToolCallId::new("call"));
    assert!(!result.result.is_error);
    assert_eq!(result.duration, Some(std::time::Duration::from_millis(7)));
    assert_eq!(echo.received.lock().unwrap().len(), 1);
    assert!(backend.events("tool.after").is_empty());
    assert_eq!(
        receiver.try_iter().collect::<Vec<_>>(),
        ["tool.after hook dispatch failed (Preparation)"]
    );
}

#[tokio::test]
async fn tool_after_interceptor_preparation_failure_withholds_native_binary_output() {
    for subscriptions in [
        vec![intercept(&["tool.after"])],
        vec![observe(&["tool.after"]), intercept(&["tool.after"])],
    ] {
        let (hooks, backend) = hooks(subscriptions);
        let outcome = call(&hooks, &Echo::new(), json!({"text": "oversized-binary"})).await;
        let ToolExecutionOutcome::Failed(ToolError::PermissionDenied(denial)) = outcome else {
            panic!("interceptor preparation failure did not withhold output");
        };
        assert_eq!(
            denial.message,
            "tool.after hook dispatch failed (Preparation)"
        );
        assert!(backend.events("tool.after").is_empty());
    }
}

#[tokio::test]
async fn tool_after_preserves_native_cancellation() {
    for subscription in [observe(&["tool.after"]), intercept(&["tool.after"])] {
        let (hooks, _) = hooks(vec![subscription]);
        assert!(matches!(
            call(&hooks, &Echo::new(), json!({"text": "cancelled"})).await,
            ToolExecutionOutcome::Failed(ToolError::Cancelled)
        ));
    }
}

#[tokio::test]
async fn tool_after_observes_the_output() {
    let (hooks, backend) = hooks(vec![observe(&["tool.after"])]);
    let echo = Echo::new();

    call(&hooks, &echo, json!({"text": "hello"})).await;

    let events = backend.events("tool.after");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["outcome"], "ok");
    assert_eq!(events[0]["items"][0]["parts"][0]["text"], "hello");
}

#[tokio::test]
async fn tool_after_reports_hook_supplied_results_as_skipped() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.before"]), observe(&["tool.after"])]);
    backend.respond(
        "tool.before",
        json!([{"type": "return", "value": "cached"}]),
    );
    let echo = Echo::new();

    call(&hooks, &echo, json!({"text": "hello"})).await;

    let events = backend.events("tool.after");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["execution"]["status"], "skipped");
    assert_eq!(events[0]["execution"]["reason"], "supplied_result");
    assert_eq!(events[0]["items"][0]["parts"][0]["text"], "cached");
}

#[tokio::test]
async fn tool_after_reports_pre_invocation_failures_as_skipped() {
    let (hooks, backend) = hooks(vec![observe(&["tool.after"])]);
    let echo = Echo::new();

    call(&hooks, &echo, json!({"text": "unavailable"})).await;

    let events = backend.events("tool.after");
    assert_eq!(events[0]["outcome"], "error");
    assert_eq!(events[0]["execution"]["status"], "skipped");
    assert_eq!(events[0]["execution"]["reason"], "other");
}

/// Answers every turn with "done" and records the transcripts it was sent.
#[derive(Clone, Default)]
struct Model {
    requests: Arc<Mutex<Vec<Vec<Item>>>>,
}

#[async_trait]
impl ModelAdapter for Model {
    type Session = Self;

    async fn start_session(&self, _: SessionConfig) -> Result<Self, LoopError> {
        Ok(self.clone())
    }
}

#[async_trait]
impl ModelSession for Model {
    type Turn = Reply;

    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        _: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        self.requests.lock().unwrap().push(request.transcript);
        Ok(Reply(Some(ModelTurnResult {
            finish_reason: FinishReason::Completed,
            output_items: vec![Item::text(ItemKind::Assistant, "done")],
            usage: None,
            metadata: MetadataMap::new(),
            model: None,
            response_id: None,
        })))
    }

    fn model_name(&self) -> Option<&str> {
        Some("model")
    }

    fn provider_name(&self) -> Option<&str> {
        Some("provider")
    }
}

struct Reply(Option<ModelTurnResult>);

#[async_trait]
impl ModelTurn for Reply {
    async fn next_event(
        &mut self,
        _: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        Ok(self.0.take().map(ModelTurnEvent::Finished))
    }
}

fn last_user_text(transcript: &[Item]) -> Option<String> {
    transcript
        .iter()
        .rev()
        .find(|item| item.kind == ItemKind::User)
        .map(|item| item.parts.iter().filter_map(part_text).collect())
}

async fn prompt(hooks: &Arc<Hooks>, model: &Model, text: &str) -> (LoopStep, Vec<Item>) {
    admit(hooks, model, vec![Item::text(ItemKind::User, text)]).await
}

async fn admit(hooks: &Arc<Hooks>, model: &Model, input: Vec<Item>) -> (LoopStep, Vec<Item>) {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("sessions");
    let initial = vec![Item::text(ItemKind::System, "system")];
    let opened =
        crate::session::open_in(root.path(), &directory, "abc", false, false, initial).unwrap();
    let mut driver = Agent::builder()
        .observer(TurnHook(hooks.clone()))
        .model(HookedAdapter::new(
            model.clone(),
            Some(hooks.clone()),
            SessionStartInputTrigger::Startup,
        ))
        .mutator(PromptHook::new(
            hooks.clone(),
            "cli",
            PromptRecord::Session(opened.observer.clone()),
        ))
        .transcript_observer(opened.observer)
        .transcript(opened.transcript)
        .input(input)
        .build()
        .unwrap()
        .start(SessionConfig::new("abc"))
        .await
        .unwrap();
    let step = driver.next().await.unwrap();
    hooks.drain_turn_events("abc").await;
    let persisted = crate::session::load_in(root.path(), &directory, "abc").unwrap();
    (step, persisted)
}

#[tokio::test]
async fn prompt_edits_replace_the_submitted_text() {
    let (hooks, backend) = hooks(vec![intercept(&["user.message.inbound"])]);
    backend.respond(
        "user.message.inbound",
        json!([{"type": "modify", "target": "prompt", "operation": "replace", "value": text_message("edited")}]),
    );
    let model = Model::default();

    let (_, persisted) = prompt(&hooks, &model, "original").await;

    let requests = model.requests.lock().unwrap();
    assert_eq!(last_user_text(&requests[0]).as_deref(), Some("edited"));
    assert_eq!(last_user_text(&persisted).as_deref(), Some("edited"));
}

#[tokio::test]
async fn denied_prompts_finish_without_a_model_call() {
    let (hooks, backend) = hooks(vec![intercept(&["user.message.inbound"])]);
    backend.respond(
        "user.message.inbound",
        json!([{"type": "deny", "reason": "blocked"}]),
    );
    let model = Model::default();

    let (step, persisted) = prompt(&hooks, &model, "original").await;

    assert!(matches!(step, LoopStep::Finished(result) if result.items.is_empty()));
    assert!(model.requests.lock().unwrap().is_empty());
    assert_eq!(last_user_text(&persisted), None);
}

#[tokio::test]
async fn media_only_prompts_reach_the_inbound_hook() {
    let (hooks, backend) = hooks(vec![intercept(&["user.message.inbound"])]);
    backend.respond(
        "user.message.inbound",
        json!([{"type": "deny", "reason": "blocked"}]),
    );
    let model = Model::default();
    let image = Item::new(
        ItemKind::User,
        vec![Part::media(
            Modality::Image,
            "image/png",
            DataRef::uri("file:///image.png"),
        )],
    );

    let (step, _) = admit(&hooks, &model, vec![image]).await;

    assert!(matches!(step, LoopStep::Finished(result) if result.items.is_empty()));
    assert!(model.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn denied_prompts_drop_their_attached_resources() {
    let (hooks, backend) = hooks(vec![intercept(&["user.message.inbound"])]);
    backend.respond(
        "user.message.inbound",
        json!([{"type": "deny", "reason": "blocked"}]),
    );
    let model = Model::default();
    let mut resource = Item::text(ItemKind::Context, "attached secret");
    resource
        .metadata
        .insert("acp.resource.uri".into(), json!("file:///secret.txt"));

    let (_, persisted) = admit(
        &hooks,
        &model,
        vec![resource, Item::text(ItemKind::User, "original")],
    )
    .await;

    assert!(persisted.iter().all(|item| item.kind == ItemKind::System));
}

#[tokio::test]
async fn model_request_denial_fails_the_turn() {
    let (hooks, backend) = hooks(vec![intercept(&["model.request.before"])]);
    backend.respond(
        "model.request.before",
        json!([{"type": "deny", "reason": "budget"}]),
    );
    let model = Model::default();
    let mut session = HookedAdapter::new(
        model.clone(),
        Some(hooks),
        SessionStartInputTrigger::Startup,
    )
    .start_session(SessionConfig::new("abc"))
    .await
    .unwrap();

    let request = TurnRequest {
        session_id: SessionId::new("abc"),
        turn_id: TurnId::new("turn"),
        transcript: vec![Item::text(ItemKind::User, "hello")],
        available_tools: Vec::new(),
        cache: None,
        metadata: MetadataMap::new(),
    };
    let error = session.begin_turn(request, None).await.err().unwrap();

    assert!(error.to_string().contains("budget"), "{error}");
    assert!(model.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn session_start_and_model_response_are_observed() {
    let (hooks, backend) = hooks(vec![observe(&["session.start", "model.response.after"])]);
    let model = Model::default();

    prompt(&hooks, &model, "hello").await;

    let started = backend.events("session.start");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["session"]["id"], "abc");
    assert_eq!(started[0]["trigger"], "startup");
    let responses = backend.events("model.response.after");
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["finishReason"], "stop");
    assert_eq!(responses[0]["items"][0]["parts"][0]["text"], "done");
}

#[tokio::test]
async fn compaction_hooks_supply_and_rewrite_the_summary() {
    let (hooks, backend) = hooks(vec![intercept(&[
        "context.compact.before",
        "context.compact.after",
    ])]);
    backend.respond(
        "context.compact.before",
        json!([{"type": "return", "value": [{"id": "s/0", "kind": "text", "mediaType": "text/plain", "selection": "body", "text": "supplied"}]}]),
    );
    let items = [Item::text(ItemKind::User, "old")];

    let before = hooks
        .compact_before("abc", &items, true, "instructions", None)
        .await;
    assert!(matches!(before, Some(Compaction::Summary(summary)) if summary == "supplied"));

    backend.respond(
        "context.compact.after",
        json!([{"type": "modify", "target": "summary", "operation": "replace", "value": [{"id": "s/0", "kind": "text", "mediaType": "text/plain", "selection": "body", "text": "rewritten"}]}]),
    );
    assert_eq!(
        hooks
            .compact_after("abc", &items, "draft".into(), false)
            .await,
        "rewritten"
    );
}

#[tokio::test]
async fn turn_boundaries_keep_native_ids_across_turns_and_sessions() {
    let (hooks, backend) = hooks(vec![observe(&["turn.start", "turn.end"])]);
    for session in ["first", "second"] {
        let mut driver = Agent::builder()
            .model(Model::default())
            .observer(TurnHook(hooks.clone()))
            .input(vec![Item::text(ItemKind::User, "one")])
            .build()
            .unwrap()
            .start(SessionConfig::new(session))
            .await
            .unwrap();
        assert!(matches!(
            driver.next().await.unwrap(),
            LoopStep::Finished(_)
        ));
        driver
            .submit_input(vec![Item::text(ItemKind::User, "two")])
            .unwrap();
        assert!(matches!(
            driver.next().await.unwrap(),
            LoopStep::Finished(_)
        ));
        hooks.drain_turn_events(session).await;
    }
    let starts = backend.events("turn.start");
    let ends = backend.events("turn.end");
    assert_eq!(starts.len(), 4);
    assert_eq!(ends.len(), 4);
    for (start, end) in starts.iter().zip(&ends) {
        assert_eq!(start["session"], end["session"]);
        assert_eq!(start["turn"], end["turn"]);
        assert_eq!(end["outcome"], "completed");
    }
    assert_eq!(starts[0]["session"]["id"], "first");
    assert_eq!(starts[2]["session"]["id"], "second");
    assert_ne!(starts[0]["turn"]["id"], starts[1]["turn"]["id"]);
}

/// Holds `turn.end` at the hook transport until the test releases it.
struct HoldTurnEnd {
    events: Arc<super::test_support::Backend>,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl agenthooksprotocol::adapters::registered::ManagedBackend for HoldTurnEnd {
    fn call(
        &self,
        request: Value,
        timeout: std::time::Duration,
    ) -> agenthooksprotocol::client::LocalFuture<
        '_,
        Result<Value, agenthooksprotocol::client::HookError>,
    > {
        Box::pin(async move {
            if request["params"]["event"]["type"] == "turn.end" {
                self.reached.notify_one();
                self.release.notified().await;
            }
            self.events.call(request, timeout).await
        })
    }

    fn shutdown(
        &self,
    ) -> agenthooksprotocol::client::LocalFuture<
        '_,
        Result<(), agenthooksprotocol::client::HookError>,
    > {
        self.events.shutdown()
    }
}

#[tokio::test]
async fn cancelled_drain_still_delivers_turn_end_before_session_end() {
    let backend = Arc::new(HoldTurnEnd {
        events: Arc::default(),
        reached: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let hooks = super::test_support::hooks_with_backend(
        vec![observe(&["turn.start", "turn.end", "session.end"])],
        backend.clone(),
    );
    let mut driver = Agent::builder()
        .model(Model::default())
        .observer(TurnHook(hooks.clone()))
        .input(vec![Item::text(ItemKind::User, "hello")])
        .build()
        .unwrap()
        .start(SessionConfig::new("held"))
        .await
        .unwrap();
    assert!(matches!(
        driver.next().await.unwrap(),
        LoopStep::Finished(_)
    ));
    tokio::select! {
        () = hooks.drain_turn_events("held") => panic!("turn.end was not held"),
        () = backend.reached.notified() => {}
    }
    backend.release.notify_one();
    hooks
        .session_end("held", SessionEndInputOutcome::Completed)
        .await;
    let types: Vec<_> = backend
        .events
        .all_events()
        .iter()
        .map(|event| event["type"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(types, ["turn.start", "turn.end", "session.end"]);
}

#[tokio::test]
async fn failed_model_request_ends_the_logical_turn() {
    let (hooks, backend) = hooks(vec![
        observe(&["turn.start", "turn.end"]),
        intercept(&["model.request.before"]),
    ]);
    backend.respond(
        "model.request.before",
        json!([{ "type": "deny", "reason": "blocked" }]),
    );
    let mut driver = Agent::builder()
        .model(HookedAdapter::new(
            Model::default(),
            Some(hooks.clone()),
            SessionStartInputTrigger::Startup,
        ))
        .observer(TurnHook(hooks.clone()))
        .input(vec![Item::text(ItemKind::User, "hello")])
        .build()
        .unwrap()
        .start(SessionConfig::new("failed"))
        .await
        .unwrap();
    assert!(driver.next().await.is_err());
    hooks.drain_turn_events("failed").await;
    let starts = backend.events("turn.start");
    let ends = backend.events("turn.end");
    assert_eq!(starts.len(), 1);
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0]["outcome"], "failed");
    assert_eq!(starts[0]["turn"], ends[0]["turn"]);
    assert_eq!(ends[0]["session"]["id"], "failed");
}

#[derive(Clone)]
struct ToolRoundModel(bool);

#[async_trait]
impl ModelAdapter for ToolRoundModel {
    type Session = Self;
    async fn start_session(&self, _: SessionConfig) -> Result<Self, LoopError> {
        Ok(self.clone())
    }
}

#[async_trait]
impl ModelSession for ToolRoundModel {
    type Turn = ToolRoundReply;
    async fn begin_turn(
        &mut self,
        _: TurnRequest,
        _: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        let mut events = std::collections::VecDeque::new();
        let call = agentkit_core::ToolCallPart::new("echo-call", "echo", json!({"text": "hello"}));
        let mut assistant = Item::text(ItemKind::Assistant, "");
        assistant.parts = vec![Part::ToolCall(call.clone())];
        if self.0 {
            // A superseded provider response is retried within this model turn.
            events.push_back(ModelTurnEvent::ToolCall(agentkit_core::ToolCallPart::new(
                "discarded-call",
                "echo",
                json!({"text": "discarded"}),
            )));
            events.push_back(ModelTurnEvent::ResponseAttemptSuperseded);
            events.push_back(ModelTurnEvent::ToolCall(call));
        }
        events.push_back(ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason: if self.0 {
                FinishReason::ToolCall
            } else {
                FinishReason::Completed
            },
            output_items: if self.0 {
                vec![assistant]
            } else {
                vec![Item::text(ItemKind::Assistant, "done")]
            },
            usage: None,
            metadata: MetadataMap::new(),
            model: None,
            response_id: None,
        }));
        self.0 = false;
        Ok(ToolRoundReply(events))
    }
}

struct ToolRoundReply(std::collections::VecDeque<ModelTurnEvent>);
#[async_trait]
impl ModelTurn for ToolRoundReply {
    async fn next_event(
        &mut self,
        _: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        Ok(self.0.pop_front())
    }
}

#[tokio::test]
async fn retry_and_tool_continuation_are_one_turn_and_retirement_cancels_once() {
    for retire in [false, true] {
        let (hooks, backend) = hooks(vec![
            observe(&["turn.start", "turn.end"]),
            intercept(&["model.request.before"]),
        ]);
        let echo = Echo::new();
        let mut registry = ToolRegistry::new();
        registry.register_arc(echo.clone());
        let mut driver = Agent::builder()
            .model(HookedAdapter::new(
                ToolRoundModel(true),
                Some(hooks.clone()),
                SessionStartInputTrigger::Startup,
            ))
            .observer(TurnHook(hooks.clone()))
            .add_tool_source(registry)
            .input(vec![Item::text(ItemKind::User, "hello")])
            .build()
            .unwrap()
            .start(SessionConfig::new("tools"))
            .await
            .unwrap();
        assert!(matches!(
            driver.next().await.unwrap(),
            LoopStep::Interrupt(agentkit_loop::LoopInterrupt::AfterToolResult(_))
        ));
        hooks.drain_turn_events("tools").await;
        assert_eq!(backend.events("turn.start").len(), 1);
        assert!(backend.events("turn.end").is_empty());
        assert_eq!(echo.received.lock().unwrap().len(), 1);
        if retire {
            assert!(driver.retire_interrupted_turn().await.unwrap().is_some());
            assert!(driver.retire_interrupted_turn().await.unwrap().is_none());
        } else {
            assert!(matches!(
                driver.next().await.unwrap(),
                LoopStep::Finished(_)
            ));
        }
        hooks.drain_turn_events("tools").await;
        let starts = backend.events("turn.start");
        let ends = backend.events("turn.end");
        assert_eq!(starts.len(), 1);
        assert_eq!(ends.len(), 1);
        assert_eq!(starts[0]["turn"], ends[0]["turn"]);
        assert_eq!(ends[0]["session"]["id"], "tools");
        assert_eq!(
            ends[0]["outcome"],
            if retire { "cancelled" } else { "completed" }
        );
        assert_eq!(
            backend.events("model.request.before").len(),
            if retire { 1 } else { 2 }
        );
    }
}

#[derive(Clone)]
struct MetadataModel;

#[async_trait]
impl ModelAdapter for MetadataModel {
    type Session = Self;
    async fn start_session(&self, _: SessionConfig) -> Result<Self, LoopError> {
        Ok(self.clone())
    }
}

#[async_trait]
impl ModelSession for MetadataModel {
    type Turn = Reply;
    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        _: Option<TurnCancellation>,
    ) -> Result<Reply, LoopError> {
        tokio::task::yield_now().await;
        if request.session_id.to_string() == "failure" {
            return Err(LoopError::ProviderFailure(Box::new(
                agentkit_loop::ProviderFailure {
                    route: agentkit_loop::ProviderRoute::OpenAiResponses,
                    reason: agentkit_loop::ProviderFailureReason::RetryExhausted,
                    last_attempt_reason: Some(agentkit_loop::ProviderFailureReason::HttpStatus),
                    upstream: agentkit_loop::ProviderClassification {
                        code: agentkit_loop::UpstreamErrorKind::RateLimitExceeded,
                        http_status: Some(429),
                        ..Default::default()
                    },
                    accounting: agentkit_loop::RetryAccounting {
                        attempts: 2,
                        ..Default::default()
                    },
                },
            )));
        }
        Ok(Reply(Some(ModelTurnResult {
            finish_reason: FinishReason::Completed,
            output_items: vec![Item::text(ItemKind::Assistant, "done")],
            usage: Some(
                agentkit_core::Usage::new(
                    agentkit_core::TokenUsage::new(12, 3)
                        .with_cached_input_tokens(4)
                        .with_cache_write_input_tokens(2)
                        .with_reasoning_tokens(1),
                )
                .with_cost(agentkit_core::CostUsage::new(0.01, "USD")),
            ),
            metadata: MetadataMap::from([("secret".into(), json!("must-not-leak"))]),
            model: Some("resolved-model".into()),
            response_id: Some(format!("response-{}", request.session_id)),
        })))
    }
    fn model_name(&self) -> Option<&str> {
        Some("requested-model")
    }
    fn provider_name(&self) -> Option<&str> {
        Some("provider")
    }
}

#[tokio::test]
async fn concurrent_model_payloads_keep_owned_context_usage_and_safe_parameters() {
    let (mut hooks, backend) = hooks(vec![intercept(&["model.request.before"]), observe(&["*"])]);
    Arc::get_mut(&mut hooks).unwrap().root = Some("/workspace".into());
    let adapter = HookedAdapter::new(
        MetadataModel,
        Some(hooks.clone()),
        SessionStartInputTrigger::Startup,
    );
    let run = |id: &'static str| {
        let adapter = adapter.clone().with_child(id != "one");
        async move {
            let mut session = adapter.start_session(SessionConfig::new(id)).await.unwrap();
            let request = TurnRequest {
                session_id: SessionId::new(id),
                turn_id: TurnId::new(format!("turn-{id}")),
                transcript: vec![Item::text(ItemKind::User, "hello")],
                available_tools: vec![Echo::new().spec().clone()],
                cache: Some(
                    agentkit_loop::PromptCacheRequest::automatic().with_key("private-routing-key"),
                ),
                metadata: MetadataMap::from([("secret".into(), json!("must-not-leak"))]),
            };
            match session.begin_turn(request, None).await {
                Ok(mut turn) => {
                    turn.next_event(None).await.unwrap();
                }
                Err(error) => assert!(matches!(error, LoopError::ProviderFailure(_))),
            }
        }
    };
    tokio::join!(run("one"), run("two"), run("failure"));
    for name in [
        "model.request.before",
        "model.response.after",
        "model.error",
    ] {
        let events = backend.events(name);
        assert!(!events.is_empty(), "missing {name}");
        for event in events {
            let session = event["session"]["id"].as_str().unwrap();
            if session == "one" {
                assert!(event["session"].get("agent").is_none());
            } else {
                assert_eq!(event["session"]["agent"], json!({"id": session}));
            }
            assert_eq!(event["turn"]["id"], format!("turn-{session}"));
            assert_eq!(event["session"]["cwd"], "/workspace");
            assert_eq!(event["session"]["workspaceRoots"], json!(["/workspace"]));
            assert_eq!(event["attempt"]["synthesized"], true);
            assert!(event["id"].is_string());
            assert!(event["time"].is_string());
            assert_eq!(event["source"], SOURCE);
            assert!(event.get("parentEventId").is_none());
            assert!(!event.to_string().contains("must-not-leak"));
            assert!(!event.to_string().contains("private-routing-key"));
            if name == "model.request.before" {
                assert_eq!(event["params"]["dev.kit.tools"][0]["name"], "echo");
                assert!(event["params"]["dev.kit.cache"].is_object());
            } else {
                assert!(event["latencyMs"].as_f64().unwrap() >= 0.0);
            }
            if name == "model.error" {
                assert_eq!(event["error"]["code"], "rate_limit_exceeded");
                assert_eq!(event["error"]["status"], 429);
            }
            if name == "model.response.after" {
                assert_eq!(event["model"]["id"], "resolved-model");
                assert_eq!(
                    event["extensions"]["dev.kit.response"]["id"],
                    format!("response-{session}")
                );
                assert_eq!(event["usage"]["inputTokens"], 12);
                assert_eq!(event["usage"]["outputTokens"], 3);
                assert_eq!(event["usage"]["cacheReadTokens"], 4);
                assert_eq!(event["usage"]["cacheWriteTokens"], 2);
                assert_eq!(event["usage"]["cost"]["basis"], "reported");
                assert_eq!(event["usage"]["scope"], "attempt");
            }
        }
    }
    for event in backend.events("session.start") {
        assert_eq!(event["session"]["model"], "requested-model");
        assert!(event["manifest"].is_object());
    }
}

#[tokio::test]
async fn concurrent_tools_keep_request_session_and_turn() {
    let (hooks, backend) = hooks(vec![intercept(&["tool.before"]), observe(&["tool.after"])]);
    let mut registry = ToolRegistry::new();
    registry.register_arc(Echo::new());
    let tool = hooked_tools(registry, &hooks)
        .get(&ToolName::new("echo"))
        .unwrap();
    let run = |id: &'static str| {
        let tool = tool.clone();
        async move {
            let context = OwnedToolContext {
                session_id: SessionId::new(id),
                turn_id: TurnId::new(format!("turn-{id}")),
                metadata: MetadataMap::new(),
                permissions: Arc::new(AllowAllPermissions),
                resources: Arc::new(()),
                cancellation: None,
                execution_scope: None,
                approved_request: None,
            };
            let request = ToolRequest::new(
                format!("call-{id}"),
                "echo",
                json!({"text": id}),
                context.session_id.clone(),
                context.turn_id.clone(),
            );
            assert!(matches!(
                tool.invoke_outcome(request, &mut context.borrowed()).await,
                ToolExecutionOutcome::Completed(_)
            ));
        }
    };
    tokio::join!(run("one"), run("two"));
    for name in ["tool.before", "tool.after"] {
        let events = backend.events(name);
        assert_eq!(events.len(), 2);
        for event in events {
            let id = event["session"]["id"].as_str().unwrap();
            assert_eq!(event["turn"]["id"], format!("turn-{id}"));
            assert_eq!(event["call"]["id"], format!("call-{id}"));
            assert_eq!(event["tool"]["input"]["text"], id);
            assert!(event["tool"].get("mcp").is_none());
            if name == "tool.after" {
                assert_eq!(event["durationMs"], 7.0);
            }
        }
    }
}

#[tokio::test]
async fn model_request_preserves_content_types_and_tool_correlations() {
    let mut subscription = intercept(&["model.request.before"]);
    // An in-process test backend has no receiver upload endpoint. Real binary
    // bodies follow the SDK's registered upload plan; metadata needs no upload.
    subscription["content"]["media"] = json!("metadata");
    let (hooks, backend) = hooks(vec![subscription]);
    let adapter = HookedAdapter::new(
        Model::default(),
        Some(hooks),
        SessionStartInputTrigger::Startup,
    );
    let mut session = adapter
        .start_session(SessionConfig::new("content"))
        .await
        .unwrap();
    let mut item = Item::text(ItemKind::User, "hello");
    item.parts.extend([
        Part::Structured(agentkit_core::StructuredPart::new(json!({"answer": 42}))),
        Part::ToolCall(agentkit_core::ToolCallPart::new(
            "call-id",
            "echo",
            json!({"text": "hi"}),
        )),
        Part::ToolResult(ToolResultPart::success("call-id", ToolOutput::text("hi"))),
        Part::Reasoning(agentkit_core::ReasoningPart::summary("readable summary")),
        Part::Media(agentkit_core::MediaPart::new(
            Modality::Image,
            "image/png",
            DataRef::inline_bytes(vec![1, 2, 3]),
        )),
        Part::File(agentkit_core::FilePart::named(
            "example.txt",
            DataRef::uri("https://example.com/file"),
        )),
        Part::Custom(agentkit_core::CustomPart::new("widget").with_value(json!({"widget": true}))),
    ]);
    session
        .begin_turn(
            TurnRequest {
                session_id: SessionId::new("content"),
                turn_id: TurnId::new("turn"),
                transcript: vec![item],
                available_tools: vec![],
                cache: None,
                metadata: MetadataMap::new(),
            },
            None,
        )
        .await
        .unwrap();
    let events = backend.events("model.request.before");
    assert_eq!(events.len(), 1);
    let parts = events[0]["items"][0]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 10);
    assert_eq!(parts[1]["category"], "structured");
    assert_eq!(
        serde_json::from_str::<Value>(parts[1]["text"].as_str().unwrap()).unwrap(),
        json!({"answer":42})
    );
    assert_eq!(
        serde_json::from_str::<Value>(parts[2]["text"].as_str().unwrap()).unwrap()["callId"],
        "call-id"
    );
    assert_eq!(
        serde_json::from_str::<Value>(parts[4]["text"].as_str().unwrap()).unwrap()["callId"],
        "call-id"
    );
    assert_eq!(parts[5]["category"], "reasoning");
    assert_eq!(parts[6]["kind"], "attachment");
    assert_eq!(parts[6]["mediaType"], "image/png");
    assert_eq!(parts[8]["text"], "example.txt");
    assert_eq!(parts[9]["category"], "custom");
    assert!(parts.iter().all(|part| part["synthesized"] == true));
}

#[tokio::test]
async fn turn_end_reports_usage_and_last_assistant_without_invented_continuations() {
    let (hooks, backend) = hooks(vec![observe(&["turn.end"])]);
    let mut driver = Agent::builder()
        .model(HookedAdapter::new(
            MetadataModel,
            Some(hooks.clone()),
            SessionStartInputTrigger::Startup,
        ))
        .observer(TurnHook(hooks.clone()))
        .input(vec![Item::text(ItemKind::User, "hello")])
        .build()
        .unwrap()
        .start(SessionConfig::new("usage"))
        .await
        .unwrap();
    assert!(matches!(
        driver.next().await.unwrap(),
        LoopStep::Finished(_)
    ));
    hooks.drain_turn_events("usage").await;
    let events = backend.events("turn.end");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["usage"]["kind"], "total");
    assert_eq!(events[0]["usage"]["scope"], "turn");
    assert_eq!(events[0]["usage"]["inputTokens"], 12);
    assert_eq!(events[0]["usage"]["cost"]["currency"], "USD");
    assert_eq!(
        events[0]["extensions"]["dev.kit.usage"]["reasoningTokens"],
        1
    );
    assert_eq!(
        events[0]["lastAssistantItem"]["id"],
        events[0]["items"][0]["id"]
    );
    assert_eq!(events[0]["continuationCount"], 0);
}

#[tokio::test]
async fn metadata_selection_does_not_leak_normalized_structured_content() {
    let mut subscription = intercept(&["model.request.before"]);
    subscription["content"]["default"] = json!("metadata");
    let (hooks, backend) = hooks(vec![subscription]);
    let adapter = HookedAdapter::new(
        Model::default(),
        Some(hooks),
        SessionStartInputTrigger::Startup,
    );
    let mut session = adapter
        .start_session(SessionConfig::new("metadata"))
        .await
        .unwrap();
    let mut item = Item::text(ItemKind::User, "private text");
    item.parts
        .push(Part::Structured(agentkit_core::StructuredPart::new(
            json!({"secret": "private value"}),
        )));
    item.parts
        .push(Part::ToolCall(agentkit_core::ToolCallPart::new(
            "call",
            "echo",
            json!({"text": "private argument"}),
        )));
    session
        .begin_turn(
            TurnRequest {
                session_id: SessionId::new("metadata"),
                turn_id: TurnId::new("turn"),
                transcript: vec![item],
                available_tools: vec![],
                cache: None,
                metadata: MetadataMap::new(),
            },
            None,
        )
        .await
        .unwrap();
    let events = backend.events("model.request.before");
    assert_eq!(events.len(), 1);
    assert!(!events[0].to_string().contains("private"));
    assert!(
        events[0]["items"][0]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|part| part["selection"] == "metadata" && part.get("text").is_none())
    );
}

#[derive(Clone)]
struct StreamUsageModel(bool);
#[async_trait]
impl ModelAdapter for StreamUsageModel {
    type Session = Self;
    async fn start_session(&self, _: SessionConfig) -> Result<Self, LoopError> {
        Ok(self.clone())
    }
}
#[async_trait]
impl ModelSession for StreamUsageModel {
    type Turn = ToolRoundReply;
    async fn begin_turn(
        &mut self,
        _: TurnRequest,
        _: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        let mut events = std::collections::VecDeque::from([ModelTurnEvent::Usage(
            agentkit_core::Usage::new(agentkit_core::TokenUsage::new(99, 99)),
        )]);
        if self.0 {
            events.push_back(ModelTurnEvent::ResponseAttemptSuperseded);
        } else {
            events.push_back(ModelTurnEvent::Usage(agentkit_core::Usage::new(
                agentkit_core::TokenUsage::new(12, 3),
            )));
        }
        events.push_back(ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason: FinishReason::Completed,
            output_items: vec![],
            usage: (!self.0).then(|| {
                agentkit_core::Usage::default().with_cost(agentkit_core::CostUsage::new(0.5, "USD"))
            }),
            metadata: MetadataMap::new(),
            model: None,
            response_id: None,
        }));
        Ok(ToolRoundReply(events))
    }
}

#[tokio::test]
async fn model_response_merges_latest_streamed_usage_and_discards_superseded_usage() {
    for superseded in [false, true] {
        let (hooks, backend) = hooks(vec![observe(&["model.response.after"])]);
        let adapter = HookedAdapter::new(
            StreamUsageModel(superseded),
            Some(hooks),
            SessionStartInputTrigger::Startup,
        );
        let mut session = adapter
            .start_session(SessionConfig::new("stream"))
            .await
            .unwrap();
        let mut turn = session
            .begin_turn(
                TurnRequest {
                    session_id: SessionId::new("stream"),
                    turn_id: TurnId::new("turn"),
                    transcript: vec![],
                    available_tools: vec![],
                    cache: None,
                    metadata: MetadataMap::new(),
                },
                None,
            )
            .await
            .unwrap();
        while turn.next_event(None).await.unwrap().is_some() {}
        let events = backend.events("model.response.after");
        assert_eq!(events.len(), 1);
        if superseded {
            assert!(events[0].get("usage").is_none());
        } else {
            assert_eq!(events[0]["usage"]["inputTokens"], 12);
            assert_eq!(events[0]["usage"]["outputTokens"], 3);
            assert_eq!(events[0]["usage"]["cost"]["amount"], 0.5);
        }
    }
}

#[tokio::test]
async fn prompt_text_edits_do_not_copy_descriptors_into_model_text() {
    for edited in ["original", "edited"] {
        let (hooks, backend) = hooks(vec![intercept(&["user.message.inbound"])]);
        let image = Part::media(
            Modality::Image,
            "image/png",
            DataRef::inline_text("data:image/png;base64,aW1hZ2U="),
        );
        let file = Part::File(agentkit_core::FilePart::named(
            "private-filename.txt",
            DataRef::uri("https://example.com/private-file"),
        ));
        let mut replacement = text_message(edited);
        for (index, part) in [&image, &file].into_iter().enumerate() {
            for descriptor in content_parts(&format!("input/{index}"), part) {
                let agenthooksprotocol::inputs::PartInput::Text(body) = descriptor else {
                    panic!("expected reference descriptor");
                };
                replacement[0]["parts"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::to_value(body).unwrap());
            }
        }
        backend.respond(
            "user.message.inbound",
            json!([{"type":"modify", "target":"prompt", "operation":"replace", "value":replacement}]),
        );
        let model = Model::default();
        let input = Item::new(
            ItemKind::User,
            vec![Part::text("original"), image.clone(), file.clone()],
        );
        let (_, persisted) = admit(&hooks, &model, vec![input]).await;
        let prompt = persisted
            .iter()
            .find(|item| item.kind == ItemKind::User)
            .unwrap();
        assert_eq!(prompt.parts, vec![Part::text(edited), image, file]);
        assert_eq!(
            last_user_text(&model.requests.lock().unwrap()[0]).as_deref(),
            Some(edited)
        );
    }
}

#[tokio::test]
async fn media_and_file_descriptors_obey_owning_category_selection() {
    for category in ["media", "file"] {
        for selection in ["omit", "metadata", "body"] {
            let mut subscription = intercept(&["model.request.before"]);
            subscription["content"]["default"] = json!("body");
            subscription["content"][category] = json!(selection);
            let (hooks, backend) = hooks(vec![subscription]);
            let adapter = HookedAdapter::new(
                Model::default(),
                Some(hooks),
                SessionStartInputTrigger::Startup,
            );
            let mut session = adapter
                .start_session(SessionConfig::new("descriptors"))
                .await
                .unwrap();
            let mut item = Item::text(ItemKind::User, "public text");
            for owner in ["media", "file"] {
                for (mime_type, data) in [
                    (
                        "image/png",
                        DataRef::uri(format!("https://example.com/{owner}-private")),
                    ),
                    (
                        "image/png",
                        DataRef::inline_text(format!("{owner}-private-base64")),
                    ),
                    (
                        "image/png",
                        DataRef::inline_text(format!("data:image/png;base64,{owner}-private")),
                    ),
                    (
                        "text/plain",
                        DataRef::inline_bytes(format!("{owner}-private-text").into_bytes()),
                    ),
                ] {
                    item.parts.push(if owner == "media" {
                        Part::media(Modality::Image, mime_type, data)
                    } else {
                        Part::File(
                            agentkit_core::FilePart::named("file-private-name.txt", data)
                                .with_mime_type(mime_type),
                        )
                    });
                }
            }
            session
                .begin_turn(
                    TurnRequest {
                        session_id: SessionId::new("descriptors"),
                        turn_id: TurnId::new("turn"),
                        transcript: vec![item],
                        available_tools: vec![],
                        cache: None,
                        metadata: MetadataMap::new(),
                    },
                    None,
                )
                .await
                .unwrap();
            let events = backend.events("model.request.before");
            assert_eq!(events.len(), 1);
            let parts = events[0]["items"][0]["parts"].as_array().unwrap();
            let selected: Vec<_> = parts
                .iter()
                .filter(|part| part["category"] == category)
                .collect();
            assert_eq!(selected.len(), if category == "media" { 4 } else { 8 });
            assert!(selected.iter().all(|part| part["selection"] == selection));
            if selection != "body" {
                assert!(selected.iter().all(|part| part.get("text").is_none()));
                assert!(
                    !events[0]
                        .to_string()
                        .contains(&format!("{category}-private"))
                );
            } else {
                assert!(
                    events[0]
                        .to_string()
                        .contains(&format!("{category}-private"))
                );
            }
            let other = if category == "media" { "file" } else { "media" };
            assert!(events[0].to_string().contains(&format!("{other}-private")));
            assert!(
                parts.iter().all(|part| matches!(
                    part["category"].as_str(),
                    Some("text" | "media" | "file")
                ))
            );
        }
    }
}

#[tokio::test]
async fn child_identity_is_session_local_through_the_hook_lifecycle() {
    let event_types = [
        "user.message.inbound",
        "session.start",
        "session.end",
        "turn.start",
        "turn.end",
        "model.request.before",
        "model.response.after",
        "tool.before",
        "tool.after",
        "context.compact.before",
        "context.compact.after",
    ];
    let (hooks, backend) = hooks(vec![observe(&event_types)]);
    let run = |id: &'static str, child| {
        let hooks = hooks.clone();
        async move {
            let mut registry = ToolRegistry::new();
            registry.register_arc(Echo::new());
            let mut driver = Agent::builder()
                .model(
                    HookedAdapter::new(
                        ToolRoundModel(true),
                        Some(hooks.clone()),
                        SessionStartInputTrigger::Startup,
                    )
                    .with_child(child),
                )
                .observer(TurnHook(hooks.clone()))
                .mutator(PromptHook::new(
                    hooks.clone(),
                    "a2a",
                    PromptRecord::Transient(crate::session::TransientTranscript::new(vec![])),
                ))
                .add_tool_source(hooked_tools(registry, &hooks))
                .input(vec![Item::text(ItemKind::User, "hello")])
                .build()
                .unwrap()
                .start(SessionConfig::new(id))
                .await
                .unwrap();
            // Both sessions remain active while model and tool events interleave.
            tokio::task::yield_now().await;
            assert!(matches!(
                driver.next().await.unwrap(),
                LoopStep::Interrupt(_)
            ));
            tokio::task::yield_now().await;
            assert!(matches!(
                driver.next().await.unwrap(),
                LoopStep::Finished(_)
            ));
            hooks.compact_before(id, &[], true, "summarize", None).await;
            hooks.compact_after(id, &[], "summary".into(), false).await;
            hooks
                .session_end(id, SessionEndInputOutcome::Completed)
                .await;
        }
    };
    tokio::join!(run("root", false), run("child", true));
    hooks.sdk.shutdown().await.unwrap();
    for event_type in event_types {
        let events = backend.events(event_type);
        for (id, child) in [("root", false), ("child", true)] {
            let events: Vec<_> = events
                .iter()
                .filter(|event| event["session"]["id"] == id)
                .collect();
            assert!(!events.is_empty(), "missing {event_type} for {id}");
            for event in events {
                if child {
                    assert_eq!(event["session"]["agent"], json!({"id": id}), "{event_type}");
                } else {
                    assert!(event["session"].get("agent").is_none(), "{event_type}");
                }
            }
        }
    }
}
