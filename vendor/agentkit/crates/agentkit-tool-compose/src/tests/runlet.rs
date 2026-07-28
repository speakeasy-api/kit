use std::sync::Arc;
use std::sync::atomic::Ordering;

use agentkit_core::{MetadataMap, SessionId, ToolCallId, ToolOutput, TurnId};
use agentkit_tools_core::{
    AllowAllPermissions, BasicToolExecutor, PermissionChecker, Tool, ToolExecutionOutcome,
    ToolExecutor, ToolInterruption, ToolName, ToolRegistry, ToolRequest,
};
use serde_json::{Value, json};

use super::{ApprovalEchoTool, EchoTool, RequireApproval, owned_context};
use crate::{COMPOSE_TOOL_NAME, ComposeConfig, ComposeTool, RunletBackend};

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

async fn execute_compose(
    config: ComposeConfig,
    child: impl Tool + 'static,
    req: ToolRequest,
) -> ToolExecutionOutcome {
    let compose = ComposeTool::new(config).with_backend(RunletBackend);
    let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::from_registry(
        ToolRegistry::new().with(compose).with(child),
    ));
    let owned = owned_context(executor.clone(), Arc::new(AllowAllPermissions));
    let mut ctx = owned.borrowed();
    executor.execute(req, &mut ctx).await
}

#[tokio::test]
async fn converts_runlet_result_to_structured_json() {
    let outcome = execute_compose(
        ComposeConfig::default(),
        EchoTool::new(),
        request(
            "return { count: input.count + 1, label: \"ok\" }",
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

#[tokio::test]
async fn tool_call_dispatches_child_tool() {
    let child = EchoTool::new();
    let calls = child.calls.clone();
    let outcome = execute_compose(
        ComposeConfig::default(),
        child,
        request(
            "out = echo({ value: input.value })\nreturn out",
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
async fn loop_fans_out_and_preserves_order() {
    let child = EchoTool::new();
    let calls = child.calls.clone();
    let outcome = execute_compose(
        ComposeConfig::default(),
        child,
        request(
            "results = for item in input.items {\n\
                 detail = echo({ value: item })\n\
                 return detail.value\n\
             }\n\
             return results",
            json!({ "items": [1, 2, 3, 4, 5, 6] }),
        ),
    )
    .await;

    match outcome {
        ToolExecutionOutcome::Completed(result) => {
            assert_eq!(
                result.result.output,
                ToolOutput::structured(json!([1, 2, 3, 4, 5, 6]))
            );
            assert_eq!(calls.load(Ordering::SeqCst), 6);
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[tokio::test]
async fn compile_diagnostics_surface_as_invalid_input() {
    let outcome = execute_compose(
        ComposeConfig::default(),
        EchoTool::new(),
        request("return missing_tool({ id: 1 })", Value::Null),
    )
    .await;

    match outcome {
        ToolExecutionOutcome::Failed(error) => {
            let message = error.to_string();
            assert!(
                message.contains("runlet program rejected"),
                "diagnostics should be model-repairable: {message}"
            );
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[tokio::test]
async fn nested_tool_call_limit_fails() {
    let outcome = execute_compose(
        ComposeConfig::default().with_max_nested_tool_calls(0),
        EchoTool::new(),
        request("return echo({ value: 1 })", Value::Null),
    )
    .await;

    assert!(matches!(outcome, ToolExecutionOutcome::Failed(_)));
}

#[tokio::test]
async fn compose_is_not_callable_from_runlet() {
    let outcome = execute_compose(
        ComposeConfig::default(),
        EchoTool::new(),
        request("return compose({ script: \"return 1\" })", Value::Null),
    )
    .await;

    // Compose filters itself out of the visible catalog, so the program
    // fails analysis with an unknown-tool diagnostic.
    assert!(matches!(outcome, ToolExecutionOutcome::Failed(_)));
}

#[tokio::test]
async fn nested_approval_interrupts_and_resumes_with_replay() {
    let compose = ComposeTool::new(ComposeConfig::default()).with_backend(RunletBackend);
    let first = EchoTool::new();
    let gated = ApprovalEchoTool::new();
    let first_calls = first.calls.clone();
    let gated_calls = gated.calls.clone();
    let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::from_registry(
        ToolRegistry::new().with(compose).with(first).with(gated),
    ));
    let permissions: Arc<dyn PermissionChecker> = Arc::new(RequireApproval);
    let req = request(
        "a = echo({ value: 1 })\n\
         b = approval_echo({ value: a.value + 1 })\n\
         return b",
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
    // The already-completed echo call replays from the record instead of
    // dispatching again; only the approved call executes.
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gated_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn computed_keys_and_object_merge_group_tool_output_into_maps() {
    let outcome = execute_compose(
        ComposeConfig::default(),
        EchoTool::new(),
        request(
            "r = echo({ value: input.rows })\n\
             by_team = fold acc = {} for p in r.value {\n\
                 return acc + { [p.team]: (acc[p.team] if p.team in acc else []) + [p.name] }\n\
             }\n\
             return by_team",
            json!({ "rows": [
                { "team": "eng", "name": "ada" },
                { "team": "ops", "name": "bo" },
                { "team": "eng", "name": "cy" }
            ] }),
        ),
    )
    .await;

    match outcome {
        ToolExecutionOutcome::Completed(result) => {
            assert_eq!(
                result.result.output,
                ToolOutput::structured(json!({ "eng": ["ada", "cy"], "ops": ["bo"] }))
            );
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[tokio::test]
async fn statement_form_if_auto_heals_and_reports_the_repair() {
    let child = EchoTool::new();
    let calls = child.calls.clone();
    // Python/JS muscle memory: statement-form if with a return-less body.
    // The healing pre-pass repairs it, the write dispatches, and the result
    // arrives wrapped with the repair notes.
    let outcome = execute_compose(
        ComposeConfig::default(),
        child,
        request(
            "flag = input.n > 1\n\
             if flag {\n\
                 r = echo({ value: input.n })\n\
             }\n\
             return flag",
            json!({ "n": 5 }),
        ),
    )
    .await;

    match outcome {
        ToolExecutionOutcome::Completed(result) => {
            let ToolOutput::Structured(value) = &result.result.output else {
                panic!("expected structured output: {:?}", result.result.output);
            };
            assert_eq!(value["value"], json!(true));
            let notes = value["compose_warnings"]["auto_repaired"]
                .as_array()
                .expect("repair notes present");
            assert!(!notes.is_empty());
            assert_eq!(calls.load(Ordering::SeqCst), 1, "the healed write ran");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[tokio::test]
async fn prelude_intrinsics_and_folds_run_locally_without_consuming_call_budget() {
    let child = EchoTool::new();
    let calls = child.calls.clone();
    // Budget of 1: the single echo call fits; the intrinsic calls and the
    // fold reductions must not count against it.
    let outcome = execute_compose(
        ComposeConfig::default().with_max_nested_tool_calls(1),
        child,
        request(
            "r = echo({ value: input.values })\n\
             flat = fold acc = [] for page in r.value { return acc + page }\n\
             assert(list.length(flat) == 3, \"unexpected flattened length\")\n\
             return {\n\
                 total: fold t = 0 for x in flat { return t + x },\n\
                 count: fold n = 0 for x in flat { return n + 1 },\n\
                 label: text.upper(text.join([\"a\", \"b\"], \"-\"))\n\
             }",
            json!({ "values": [[1, 2], [3]] }),
        ),
    )
    .await;

    match outcome {
        ToolExecutionOutcome::Completed(result) => {
            assert_eq!(
                result.result.output,
                ToolOutput::structured(json!({ "total": 6, "count": 3, "label": "A-B" }))
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}
