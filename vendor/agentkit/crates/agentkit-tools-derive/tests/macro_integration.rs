//! Integration test for the `#[tool]` attribute macro.
//!
//! Exercises name/description defaults, name override, doc-comment-as-
//! description, schema derivation, and the round-trip through
//! `Tool::invoke`.

use agentkit_core::{MetadataMap, SessionId, ToolCallId, ToolOutput, ToolResultPart, TurnId};
use agentkit_tools_core::{
    AllowAllPermissions, OwnedToolContext, Tool, ToolError, ToolName, ToolRequest, ToolResult,
};
use agentkit_tools_derive::tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(JsonSchema, Deserialize)]
struct GreetInput {
    /// Person to greet.
    name: String,
}

/// Greet a person by name.
#[tool]
async fn greet(input: GreetInput) -> Result<ToolResult, ToolError> {
    Ok(ToolResult::new(ToolResultPart::success(
        ToolCallId::new("greet-call"),
        ToolOutput::text(format!("hello, {}", input.name)),
    )))
}

#[derive(JsonSchema, Deserialize)]
struct EchoInput {
    value: String,
}

#[tool(name = "speak", description = "Echo the input value back")]
async fn echo(input: EchoInput) -> Result<ToolResult, ToolError> {
    Ok(ToolResult::new(ToolResultPart::success(
        ToolCallId::new("echo-call"),
        ToolOutput::text(input.value),
    )))
}

fn build_request(tool_name: &str, input: serde_json::Value) -> ToolRequest {
    ToolRequest {
        call_id: ToolCallId::new("c"),
        tool_name: ToolName::new(tool_name),
        input,
        session_id: SessionId::new("s"),
        turn_id: TurnId::new("t"),
        metadata: MetadataMap::new(),
    }
}

fn ctx() -> OwnedToolContext {
    OwnedToolContext {
        session_id: SessionId::new("s"),
        turn_id: TurnId::new("t"),
        metadata: MetadataMap::new(),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: None,
        execution_scope: None,
        approved_request: None,
    }
}

#[tokio::test]
async fn tool_attribute_defaults_name_to_function_ident() {
    let spec = greet.spec();
    assert_eq!(spec.name.0, "greet");
    assert_eq!(spec.description, "Greet a person by name.");
    assert!(spec.input_schema.is_object());
    assert!(
        spec.input_schema
            .get("properties")
            .and_then(|p| p.get("name"))
            .is_some()
    );
}

#[tokio::test]
async fn tool_attribute_invoke_decodes_input_and_returns_result() {
    let owned = ctx();
    let mut ctx = owned.borrowed();
    let request = build_request("greet", json!({ "name": "world" }));
    let result = greet.invoke(request, &mut ctx).await.unwrap();
    match result.result.output {
        ToolOutput::Text(text) => assert_eq!(text, "hello, world"),
        other => panic!("unexpected output: {other:?}"),
    }
}

#[tokio::test]
async fn tool_attribute_explicit_name_and_description_override() {
    let spec = echo.spec();
    assert_eq!(spec.name.0, "speak");
    assert_eq!(spec.description, "Echo the input value back");
}

#[tokio::test]
async fn tool_attribute_invoke_propagates_invalid_input_error() {
    let owned = ctx();
    let mut ctx = owned.borrowed();
    let request = build_request("greet", json!({ "wrong_field": "world" }));
    let err = greet.invoke(request, &mut ctx).await.unwrap_err();
    assert!(matches!(err, ToolError::InvalidInput(_)));
}

#[derive(JsonSchema, Deserialize)]
struct AnnotatedInput {
    value: String,
}

/// Tool with every annotation flag set so we can assert each maps onto the
/// matching ToolAnnotations field.
#[tool(
    read_only,
    destructive = false,
    idempotent,
    needs_approval = true,
    supports_streaming
)]
async fn annotated(input: AnnotatedInput) -> Result<ToolResult, ToolError> {
    Ok(ToolResult::new(ToolResultPart::success(
        ToolCallId::new("annotated"),
        ToolOutput::text(input.value),
    )))
}

#[tokio::test]
async fn tool_attribute_flags_set_annotations() {
    let spec = annotated.spec();
    assert!(spec.annotations.read_only_hint);
    assert!(!spec.annotations.destructive_hint);
    assert!(spec.annotations.idempotent_hint);
    assert!(spec.annotations.needs_approval_hint);
    assert!(spec.annotations.supports_streaming_hint);
}

#[derive(JsonSchema, Deserialize)]
struct CounterInput {
    delta: i64,
}

pub struct Counter {
    pub start: i64,
}

#[tool(idempotent)]
impl Counter {
    /// Add the delta to the configured starting value.
    async fn run(&self, input: CounterInput) -> Result<ToolResult, ToolError> {
        let total = self.start + input.delta;
        Ok(ToolResult::new(ToolResultPart::success(
            ToolCallId::new("counter-call"),
            ToolOutput::text(format!("{total}")),
        )))
    }
}

#[tokio::test]
async fn tool_attribute_impl_form_uses_struct_state() {
    let counter = Counter { start: 10 };
    let spec = agentkit_tools_core::Tool::spec(&counter);
    assert_eq!(spec.name.0, "run");
    assert_eq!(
        spec.description,
        "Add the delta to the configured starting value."
    );
    assert!(spec.annotations.idempotent_hint);

    let owned = ctx();
    let mut ctx = owned.borrowed();
    let request = build_request("run", json!({ "delta": 5 }));
    let result = agentkit_tools_core::Tool::invoke(&counter, request, &mut ctx)
        .await
        .unwrap();
    match result.result.output {
        ToolOutput::Text(text) => assert_eq!(text, "15"),
        other => panic!("unexpected output: {other:?}"),
    }
    assert_eq!(result.result.call_id.0, "c");
}

#[derive(JsonSchema, Deserialize)]
struct NamedInput {
    label: String,
}

pub struct Renamer;

#[tool(
    name = "rename_thing",
    description = "Override description.",
    destructive
)]
impl Renamer {
    async fn whatever(&self, input: NamedInput) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::new(ToolResultPart::success(
            ToolCallId::new("renamer"),
            ToolOutput::text(input.label),
        )))
    }
}

#[tokio::test]
async fn tool_attribute_impl_form_respects_overrides() {
    let renamer = Renamer;
    let spec = agentkit_tools_core::Tool::spec(&renamer);
    assert_eq!(spec.name.0, "rename_thing");
    assert_eq!(spec.description, "Override description.");
    assert!(spec.annotations.destructive_hint);
}
