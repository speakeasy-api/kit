//! End-to-end test for the `#[tool]` proc macro and `tool_spec_for`
//! schemars helper.
//!
//! The unit tests in `crates/agentkit-tools-derive/tests/` exercise
//! `.spec()` and `.invoke()` directly. This test runs a macro-defined tool
//! through a real `LoopDriver`: it asserts that
//!
//! 1. the catalog the loop hands the model includes the macro-derived
//!    `ToolSpec` with a JSON-schema input schema (proves the schemars
//!    pathway flows through);
//! 2. the model's tool-call args round-trip through serde + the macro
//!    decoder + the user's function body and land in the transcript as a
//!    `Tool/ToolResult` with the expected payload (proves the runtime
//!    wiring between the macro and the loop is correct).

use agentkit_core::{
    FinishReason, Item, ItemKind, Part, ToolCallId, ToolCallPart, ToolOutput, ToolResultPart,
};
use agentkit_integration_tests::mock_model::{MockAdapter, TurnScript};
use agentkit_loop::{Agent, LoopInterrupt, LoopStep, SessionConfig};
use agentkit_tools_core::{ToolError, ToolRegistry, ToolResult};
use agentkit_tools_derive::tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

#[derive(JsonSchema, Deserialize)]
struct CityInput {
    /// City name to look up.
    city: String,
    /// Use celsius (default false).
    #[serde(default)]
    celsius: bool,
}

/// Fetch the current weather for a city.
#[tool]
async fn get_weather(input: CityInput) -> Result<ToolResult, ToolError> {
    let unit = if input.celsius { "C" } else { "F" };
    Ok(ToolResult::new(ToolResultPart::success(
        ToolCallId::new("placeholder"),
        ToolOutput::text(format!("sunny in {} ({unit})", input.city)),
    )))
}

#[tokio::test]
async fn tool_macro_runs_end_to_end_through_loop_driver() {
    let call = ToolCallPart::new(
        ToolCallId::new("call-1"),
        "get_weather",
        json!({ "city": "Berlin", "celsius": true }),
    );

    let adapter = MockAdapter::new();
    adapter.enqueue(TurnScript::tool_call(call));
    adapter.enqueue(TurnScript::text("done"));

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(ToolRegistry::new().with(get_weather))
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new("tool-macro-end-to-end"),
        vec![Item::text(ItemKind::User, "weather please")],
    )
    .await;

    // Drive the loop to completion. AfterToolResult cooperates by just
    // calling next() again (no interjection); we do not expect approval.
    loop {
        match driver.next().await.unwrap() {
            LoopStep::Finished(turn) => {
                assert_eq!(turn.finish_reason, FinishReason::Completed);
                break;
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                panic!("unexpected AwaitingInput before Finished")
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(_)) => {
                panic!("unexpected approval interrupt")
            }
        }
    }

    // 1. The catalog handed to the model on turn 1 must include the macro-
    //    derived `get_weather` tool. The mock records tool names per turn;
    //    full schema-shape assertions live in the tools-core unit tests.
    let observed = adapter.observed();
    let first = observed.first().expect("at least one observed turn");
    assert!(
        first.tool_names.iter().any(|n| n == "get_weather"),
        "get_weather advertised to model on turn 1, got: {:?}",
        first.tool_names,
    );

    // The macro-generated spec must carry the doc comment as description
    // and a JSON-schema input schema with the typed fields.
    let spec = agentkit_tools_core::Tool::spec(&get_weather);
    assert_eq!(spec.description, "Fetch the current weather for a city.");
    let schema = spec.input_schema.as_object().expect("schema is an object");
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .expect("schema has properties");
    assert!(properties.contains_key("city"));
    assert!(properties.contains_key("celsius"));

    // 2. The transcript must carry the decoded tool result. The macro
    //    decoded `{"city": "Berlin", "celsius": true}` and the user body
    //    formatted it as `sunny in Berlin (C)`.
    let transcript = driver.snapshot().transcript;
    let tool_result_text = transcript
        .iter()
        .filter(|item| item.kind == ItemKind::Tool)
        .flat_map(|item| item.parts.iter())
        .find_map(|part| match part {
            Part::ToolResult(result) => match &result.output {
                ToolOutput::Text(text) => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("transcript contains a Tool/ToolResult with text output");
    assert_eq!(tool_result_text, "sunny in Berlin (C)");
}

#[tokio::test]
async fn tool_macro_invalid_input_surfaces_as_error_tool_result() {
    let call = ToolCallPart::new(
        ToolCallId::new("call-bad"),
        "get_weather",
        json!({ "celsius": true }), // missing required `city`
    );

    let adapter = MockAdapter::new();
    adapter.enqueue(TurnScript::tool_call(call));
    adapter.enqueue(TurnScript::text("acknowledged the error"));

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(ToolRegistry::new().with(get_weather))
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new("tool-macro-invalid-input"),
        vec![Item::text(ItemKind::User, "missing field")],
    )
    .await;

    loop {
        match driver.next().await.unwrap() {
            LoopStep::Finished(turn) => {
                assert_eq!(turn.finish_reason, FinishReason::Completed);
                break;
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(other) => panic!("unexpected interrupt: {other:?}"),
        }
    }

    // The decoder rejected the input — the transcript carries a
    // Tool/ToolResult with `is_error = true`.
    let transcript = driver.snapshot().transcript;
    let error_result = transcript
        .iter()
        .filter(|item| item.kind == ItemKind::Tool)
        .flat_map(|item| item.parts.iter())
        .find_map(|part| match part {
            Part::ToolResult(result) if result.is_error => Some(result.clone()),
            _ => None,
        })
        .expect("transcript contains an error Tool/ToolResult");
    match error_result.output {
        ToolOutput::Text(text) => {
            assert!(
                text.contains("missing") || text.contains("required") || text.contains("city"),
                "error message references the missing field, got: {text}"
            );
        }
        other => panic!("expected text error output, got: {other:?}"),
    }
}
