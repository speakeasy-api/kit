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
        .session_start("environment-test", SessionStartInputTrigger::Startup)
        .await;
    hooks.sdk.shutdown().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(output).unwrap(),
        "1\nherdr-bin-sentinel\nconfigured-pane\nsocket-sentinel\nenvironment-sentinel\n",
    );
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
        .compact_before(&items, true, "instructions", None)
        .await;
    hooks.compact_after(&items, "summary".into()).await;

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
        self.received.lock().unwrap().push(request.input.clone());
        let text = request.input["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::text(text),
        )))
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
        .compact_before(&items, true, "instructions", None)
        .await;
    assert!(matches!(before, Some(Compaction::Summary(summary)) if summary == "supplied"));

    backend.respond(
        "context.compact.after",
        json!([{"type": "modify", "target": "summary", "operation": "replace", "value": [{"id": "s/0", "kind": "text", "mediaType": "text/plain", "selection": "body", "text": "rewritten"}]}]),
    );
    assert_eq!(
        hooks.compact_after(&items, "draft".into()).await,
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
