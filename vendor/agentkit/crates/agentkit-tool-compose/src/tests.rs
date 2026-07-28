use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use agentkit_core::{ApprovalId, SessionId, TurnId};
use agentkit_tools_core::{
    AllowAllPermissions, ApprovalReason, ApprovalRequest, BasicToolExecutor, PermissionChecker,
    PermissionDecision, PermissionRequest, ToolExecutionScope, ToolExecutor,
};
use serde_json::json;

use super::*;

#[derive(Clone)]
pub(crate) struct EchoTool {
    spec: ToolSpec,
    pub(crate) calls: Arc<AtomicUsize>,
}

impl EchoTool {
    pub(crate) fn new() -> Self {
        Self {
            spec: ToolSpec::new("echo", "echo input", json!({"type": "object"})),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(request.input),
        )))
    }
}

struct ApprovalPermissionRequest {
    metadata: MetadataMap,
}

impl PermissionRequest for ApprovalPermissionRequest {
    fn kind(&self) -> &'static str {
        "compose.test.approval"
    }

    fn summary(&self) -> String {
        "approval required".into()
    }

    fn metadata(&self) -> &MetadataMap {
        &self.metadata
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone)]
pub(crate) struct ApprovalEchoTool {
    spec: ToolSpec,
    pub(crate) calls: Arc<AtomicUsize>,
}

impl ApprovalEchoTool {
    pub(crate) fn new() -> Self {
        Self {
            spec: ToolSpec::new("approval_echo", "approval echo", json!({"type": "object"})),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl Tool for ApprovalEchoTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn proposed_requests(
        &self,
        _request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        Ok(vec![Box::new(ApprovalPermissionRequest {
            metadata: MetadataMap::new(),
        })])
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(request.input),
        )))
    }
}

pub(crate) struct RequireApproval;

impl PermissionChecker for RequireApproval {
    fn evaluate(&self, request: &dyn PermissionRequest) -> PermissionDecision {
        PermissionDecision::RequireApproval(ApprovalRequest {
            task_id: None,
            call_id: None,
            id: ApprovalId::new("approval:test"),
            request_kind: request.kind().into(),
            reason: ApprovalReason::PolicyRequiresConfirmation,
            summary: request.summary(),
            metadata: request.metadata().clone(),
        })
    }
}

fn request(script: &str, input: Value) -> ToolRequest {
    ToolRequest {
        call_id: ToolCallId::new("compose-call"),
        tool_name: ToolName::new(COMPOSE_TOOL_NAME),
        input: json!({ "script": script, "input": input }),
        session_id: SessionId::new("session"),
        turn_id: TurnId::new("turn"),
        metadata: MetadataMap::new(),
    }
}

pub(crate) fn owned_context(
    executor: Arc<dyn ToolExecutor>,
    permissions: Arc<dyn PermissionChecker>,
) -> agentkit_tools_core::OwnedToolContext {
    let session_id = SessionId::new("session");
    let turn_id = TurnId::new("turn");
    let metadata = MetadataMap::new();
    let resources: Arc<dyn agentkit_tools_core::ToolResources> = Arc::new(());
    let scope = ToolExecutionScope {
        executor,
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        permissions: permissions.clone(),
        resources: resources.clone(),
        cancellation: None,
    };
    agentkit_tools_core::OwnedToolContext {
        session_id,
        turn_id,
        metadata,
        permissions,
        resources,
        cancellation: None,
        execution_scope: Some(scope),
        approved_request: None,
    }
}

async fn execute_compose(
    config: ComposeConfig,
    child: impl Tool + 'static,
    req: ToolRequest,
) -> ToolExecutionOutcome {
    let compose = ComposeTool::new(config);
    let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::from_registry(
        ToolRegistry::new().with(compose).with(child),
    ));
    let owned = owned_context(executor.clone(), Arc::new(AllowAllPermissions));
    let mut ctx = owned.borrowed();
    executor.execute(req, &mut ctx).await
}

#[tokio::test]
async fn converts_lua_result_to_structured_json() {
    let outcome = execute_compose(
        ComposeConfig::default(),
        EchoTool::new(),
        request(
            "return { count = input.count + 1, label = 'ok' }",
            json!({ "count": 2 }),
        ),
    )
    .await;

    match outcome {
        ToolExecutionOutcome::Completed(result) => {
            assert_eq!(
                result.result.output,
                ToolOutput::structured(json!({ "count": 3, "label": "ok" }))
            );
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[cfg(feature = "toon")]
#[tokio::test]
async fn toon_encoding_returns_text_result() {
    let outcome = execute_compose(
        ComposeConfig::default().with_result_encoding(ResultEncoding::Toon),
        EchoTool::new(),
        request(
            "return { total = 2, items = { { id = 'a', n = 1 }, { id = 'b', n = 2 } } }",
            json!(null),
        ),
    )
    .await;

    match outcome {
        ToolExecutionOutcome::Completed(result) => {
            let ToolOutput::Text(text) = &result.result.output else {
                panic!("expected TOON text output, got {:?}", result.result.output);
            };
            assert!(text.contains("total: 2"), "scalar field missing: {text}");
            // Key order inside the header follows serde_json's map order,
            // which flips to Lua-table insertion order when some other crate
            // in the build enables `preserve_order` — accept either.
            assert!(
                text.contains("items[2]{id,n}:") || text.contains("items[2]{n,id}:"),
                "uniform list should render tabular: {text}"
            );
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[cfg(feature = "toon")]
#[test]
fn toon_encoding_adds_description_note() {
    let toon =
        ComposeTool::new(ComposeConfig::default().with_result_encoding(ResultEncoding::Toon));
    assert!(
        toon.spec()
            .description
            .contains("Token-Oriented Object Notation")
    );

    let default = ComposeTool::new(ComposeConfig::default());
    assert!(
        !default
            .spec()
            .description
            .contains("Token-Oriented Object Notation")
    );
}

#[tokio::test]
async fn tool_function_calls_child_tool() {
    let child = EchoTool::new();
    let calls = child.calls.clone();
    let outcome = execute_compose(
        ComposeConfig::default(),
        child,
        request(
            "local out = tool('echo', { value = input.value }); return out",
            json!({ "value": 7 }),
        ),
    )
    .await;

    match outcome {
        ToolExecutionOutcome::Completed(result) => {
            assert_eq!(
                result.result.output,
                ToolOutput::structured(json!({ "value": 7 }))
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[tokio::test]
async fn tools_excludes_compose_by_default() {
    let outcome = execute_compose(
        ComposeConfig::default(),
        EchoTool::new(),
        request(
            "for _, spec in ipairs(tools()) do if spec.name == 'compose' then return 'bad' end end; return 'ok'",
            Value::Null,
        ),
    )
    .await;

    match outcome {
        ToolExecutionOutcome::Completed(result) => {
            assert_eq!(result.result.output, ToolOutput::structured(json!("ok")));
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[tokio::test]
async fn sandbox_removes_os_io_and_require() {
    for script in [
        "return os.getenv('HOME')",
        "return io.open('Cargo.toml')",
        "return require('x')",
    ] {
        let outcome = execute_compose(
            ComposeConfig::default(),
            EchoTool::new(),
            request(script, Value::Null),
        )
        .await;
        assert!(matches!(outcome, ToolExecutionOutcome::Failed(_)));
    }
}

#[tokio::test]
async fn nested_tool_call_limit_fails() {
    let outcome = execute_compose(
        ComposeConfig::default().with_max_nested_tool_calls(0),
        EchoTool::new(),
        request("return tool('echo', {})", Value::Null),
    )
    .await;

    assert!(matches!(outcome, ToolExecutionOutcome::Failed(_)));
}

#[tokio::test]
async fn instruction_limit_fails() {
    let outcome = execute_compose(
        ComposeConfig::default().with_max_instruction_count(25),
        EchoTool::new(),
        request(
            "local x = 0; for i = 1, 100000 do x = x + 1 end; return x",
            Value::Null,
        ),
    )
    .await;

    assert!(matches!(outcome, ToolExecutionOutcome::Failed(_)));
}

#[tokio::test]
async fn nested_approval_replays_completed_children_once() {
    let compose = ComposeTool::new(ComposeConfig::default());
    let states = compose.states.clone();
    let first = EchoTool::new();
    let gated = ApprovalEchoTool::new();
    let first_calls = first.calls.clone();
    let gated_calls = gated.calls.clone();
    let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::from_registry(
        ToolRegistry::new().with(compose).with(first).with(gated),
    ));
    let permissions: Arc<dyn PermissionChecker> = Arc::new(RequireApproval);
    let req = request(
        "local a = tool('echo', { value = 1 }); local b = tool('approval_echo', { value = a.value + 1 }); return b",
        Value::Null,
    );

    let owned = owned_context(executor.clone(), permissions.clone());
    let mut ctx = owned.borrowed();
    let first_outcome = executor.execute(req.clone(), &mut ctx).await;
    let approval = match first_outcome {
        ToolExecutionOutcome::Interrupted(ToolInterruption::ApprovalRequired(approval)) => approval,
        other => panic!("unexpected first outcome: {other:?}"),
    };
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gated_calls.load(Ordering::SeqCst), 0);
    // After an approval interrupt, the per-call replay state must persist so
    // the resumed run can replay completed children and re-issue the
    // pending one.
    assert!(
        !states.lock().await.is_empty(),
        "compose run state must be retained across approval interrupts"
    );

    let owned = owned_context(executor.clone(), permissions);
    let outcome = executor.execute_approved_owned(req, &approval, owned).await;
    match outcome {
        ToolExecutionOutcome::Completed(result) => {
            assert_eq!(
                result.result.output,
                ToolOutput::structured(json!({ "value": 2 }))
            );
        }
        other => panic!("unexpected approved outcome: {other:?}"),
    }
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gated_calls.load(Ordering::SeqCst), 1);
    // Once the compose run completes, the state-map entry must be cleared.
    assert!(
        states.lock().await.is_empty(),
        "compose run state must be cleared after a successful resume"
    );
}

#[tokio::test]
async fn state_map_cleared_after_successful_run() {
    let compose = ComposeTool::new(ComposeConfig::default());
    let states = compose.states.clone();
    let child = EchoTool::new();
    let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::from_registry(
        ToolRegistry::new().with(compose).with(child),
    ));
    let owned = owned_context(executor.clone(), Arc::new(AllowAllPermissions));
    let mut ctx = owned.borrowed();
    let outcome = executor
        .execute(
            request("return tool('echo', { value = 1 })", Value::Null),
            &mut ctx,
        )
        .await;
    assert!(matches!(outcome, ToolExecutionOutcome::Completed(_)));
    assert!(
        states.lock().await.is_empty(),
        "compose run state must be cleared after a successful run"
    );
}

#[tokio::test]
async fn state_map_cleared_after_script_eval_failure() {
    // Regression: an oversized result returned from the Lua script used
    // to leak the state-map entry created during the run's setup.
    let compose = ComposeTool::new(ComposeConfig::default().with_max_result_bytes(1));
    let states = compose.states.clone();
    let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::from_registry(
        ToolRegistry::new().with(compose).with(EchoTool::new()),
    ));
    let owned = owned_context(executor.clone(), Arc::new(AllowAllPermissions));
    let mut ctx = owned.borrowed();
    let outcome = executor
        .execute(
            request(
                "return 'this string is far longer than one byte'",
                Value::Null,
            ),
            &mut ctx,
        )
        .await;
    assert!(
        matches!(outcome, ToolExecutionOutcome::Failed(_)),
        "expected oversized compose result to fail",
    );
    assert!(
        states.lock().await.is_empty(),
        "compose run state must be cleared after a script-eval failure"
    );
}

#[tokio::test]
async fn concurrent_runs_over_disjoint_call_ids() {
    let compose = ComposeTool::new(ComposeConfig::default());
    let child = EchoTool::new();
    let child_calls = child.calls.clone();
    let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::from_registry(
        ToolRegistry::new().with(compose).with(child),
    ));

    let make_request = |call: &str, base: i64| ToolRequest {
        call_id: ToolCallId::new(call),
        tool_name: ToolName::new(COMPOSE_TOOL_NAME),
        input: json!({
            "script": "local a = tool('echo', { value = input.base }); local b = tool('echo', { value = a.value + 1 }); return { a = a.value, b = b.value }",
            "input": { "base": base },
        }),
        session_id: SessionId::new("session"),
        turn_id: TurnId::new("turn"),
        metadata: MetadataMap::new(),
    };

    let permissions: Arc<dyn PermissionChecker> = Arc::new(AllowAllPermissions);

    let executor_a = executor.clone();
    let permissions_a = permissions.clone();
    let req_a = make_request("compose-call-a", 10);
    let handle_a = tokio::spawn(async move {
        let owned = owned_context(executor_a.clone(), permissions_a);
        let mut ctx = owned.borrowed();
        executor_a.execute(req_a, &mut ctx).await
    });

    let executor_b = executor.clone();
    let permissions_b = permissions.clone();
    let req_b = make_request("compose-call-b", 100);
    let handle_b = tokio::spawn(async move {
        let owned = owned_context(executor_b.clone(), permissions_b);
        let mut ctx = owned.borrowed();
        executor_b.execute(req_b, &mut ctx).await
    });

    let outcome_a = handle_a.await.expect("compose A join");
    let outcome_b = handle_b.await.expect("compose B join");

    match outcome_a {
        ToolExecutionOutcome::Completed(result) => assert_eq!(
            result.result.output,
            ToolOutput::structured(json!({ "a": 10, "b": 11 }))
        ),
        other => panic!("unexpected outcome A: {other:?}"),
    }
    match outcome_b {
        ToolExecutionOutcome::Completed(result) => assert_eq!(
            result.result.output,
            ToolOutput::structured(json!({ "a": 100, "b": 101 }))
        ),
        other => panic!("unexpected outcome B: {other:?}"),
    }

    // Two compose runs, each making two nested calls.
    assert_eq!(child_calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn unadvertised_children_dispatch_without_advertisement() {
    let child = EchoTool::new();
    let child_calls = child.calls.clone();
    let compose = ComposeTool::new(ComposeConfig::default())
        .with_source(ToolRegistry::new().with(child).unadvertised());

    let specs = ToolSource::specs(&compose);
    let names: Vec<_> = specs.iter().map(|s| s.name.0.as_str()).collect();
    assert_eq!(names, vec![COMPOSE_TOOL_NAME]);
    assert!(
        !specs[0].description.contains("echo"),
        "hidden child must not be enumerated in the compose description"
    );
    assert!(
        ToolSource::get(&compose, &ToolName::new("echo")).is_some(),
        "hidden child stays resolvable for dispatch"
    );

    let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::new([
        Arc::new(compose) as Arc<dyn ToolSource>
    ]));
    let owned = owned_context(executor.clone(), Arc::new(AllowAllPermissions));
    let mut ctx = owned.borrowed();
    let outcome = executor
        .execute(
            request("return tool('echo', { value = 7 })", Value::Null),
            &mut ctx,
        )
        .await;
    match outcome {
        ToolExecutionOutcome::Completed(result) => assert_eq!(
            result.result.output,
            ToolOutput::structured(json!({ "value": 7 }))
        ),
        other => panic!("unexpected outcome: {other:?}"),
    }
    assert_eq!(child_calls.load(Ordering::SeqCst), 1);
}

/// A dynamic source whose advertised description can change at runtime
/// and whose pending catalog events are surfaced through
/// `drain_catalog_events`.
struct MutableSource {
    spec: StdMutex<ToolSpec>,
    pending_event: AtomicBool,
    specs_calls: Arc<AtomicUsize>,
}

impl ToolSource for MutableSource {
    fn specs(&self) -> Vec<ToolSpec> {
        self.specs_calls.fetch_add(1, Ordering::SeqCst);
        vec![self.spec.lock().expect("spec lock").clone()]
    }

    fn get(&self, _name: &ToolName) -> Option<Arc<dyn Tool>> {
        None
    }

    fn drain_catalog_events(&self) -> Vec<ToolCatalogEvent> {
        if self.pending_event.swap(false, Ordering::AcqRel) {
            let mut event = ToolCatalogEvent::new("mutable");
            event.changed.push("echo".into());
            vec![event]
        } else {
            Vec::new()
        }
    }
}

#[test]
fn compose_spec_is_cached_until_catalog_events() {
    let specs_calls = Arc::new(AtomicUsize::new(0));
    let source = Arc::new(MutableSource {
        spec: StdMutex::new(
            ToolSpec::new("echo", "echo input", json!({"type": "object"}))
                .with_output_schema(json!({"type": "string"})),
        ),
        pending_event: AtomicBool::new(false),
        specs_calls: specs_calls.clone(),
    });
    let compose = ComposeTool::new(ComposeConfig::default()).with_source(source.clone());

    let baseline = specs_calls.load(Ordering::SeqCst);
    let first = ToolSource::specs(&compose);
    // One walk for the live child list only — the compose description is
    // served from cache, not re-rendered from another source.specs() pass.
    assert_eq!(specs_calls.load(Ordering::SeqCst), baseline + 1);
    let second = ToolSource::specs(&compose);
    assert_eq!(specs_calls.load(Ordering::SeqCst), baseline + 2);
    assert_eq!(first[0].description, second[0].description);

    // Change the child's output schema without an event: the cached
    // compose description intentionally stays as-is (the model's catalog
    // would not refresh either).
    *source.spec.lock().expect("spec lock") =
        ToolSpec::new("echo", "echo input", json!({"type": "object"}))
            .with_output_schema(json!({"type": "number"}));
    let stale = ToolSource::specs(&compose);
    assert!(stale[0].description.contains("echo: string"));

    // After the source reports a catalog event, the description refreshes.
    source.pending_event.store(true, Ordering::Release);
    let events = ToolSource::drain_catalog_events(&compose);
    assert!(!events.is_empty());
    let fresh = ToolSource::specs(&compose);
    assert!(fresh[0].description.contains("echo: number"));
}

#[test]
fn type_notation_renders_json_schema_compactly() {
    // A bare array must read as `T[]`, never expose the schema keyword
    // `items` a model could mistake for a property.
    assert_eq!(
        type_notation(&json!({"type": "array", "items": {"type": "string"}}), 0),
        "string[]"
    );
    assert_eq!(
        type_notation(
            &json!({
                "type": "object",
                "properties": {
                    "items": {"type": "array", "items": {"type": "object", "properties": {
                        "id": {"type": "string"},
                        "start": {"type": "string", "description": "HH:MM UTC"},
                        "level": {"type": "string", "enum": ["INFO", "ERROR"]}
                    }, "required": ["id"]}},
                    "total_pages": {"type": "integer"},
                    "cursor": {"type": "string"}
                },
                "required": ["items", "total_pages"]
            }),
            0,
        ),
        "{ \"cursor\"?: string, \"items\": { \"id\": string, \"level\"?: \"INFO\" | \"ERROR\", \"start\"?: string /* HH:MM UTC */ }[], \"total_pages\": int }"
    );
    assert_eq!(
        type_notation(
            &json!({"type": "object", "additionalProperties": {"type": "number"}}),
            0
        ),
        "{ [key]: number }"
    );
}

#[cfg(feature = "runlet")]
mod runlet;
