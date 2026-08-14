//! Lifecycle reporting for the hidden tools behind `compose`.
//!
//! The wrapper is transparent to the model and to compose: it forwards the
//! spec, permission requests, and invocation untouched, and only publishes
//! start/finish events on the runtime side channel (see [`crate::events`]) so
//! a client can draw what a Runlet program is doing while it runs.

use std::time::Instant;

use agentkit_core::ToolOutput;
use agentkit_tools_core::{
    PermissionRequest, Tool, ToolContext, ToolError, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;

use serde_json::{Value, json};

use crate::events::{self, RuntimeEvent, summarize_input, summarize_output};

/// Wraps a tool so its calls appear on the runtime side channel.
pub struct Observed<T>(T);

impl<T: Tool> Observed<T> {
    pub const fn new(tool: T) -> Self {
        Self(tool)
    }
}

#[async_trait]
impl<T: Tool> Tool for Observed<T> {
    fn spec(&self) -> &ToolSpec {
        self.0.spec()
    }

    fn current_spec(&self) -> Option<ToolSpec> {
        self.0.current_spec()
    }

    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        self.0.proposed_requests(request)
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        if !events::enabled() {
            return self.0.invoke(request, context).await;
        }
        let call = request.call_id.0.clone();
        let tool = request.tool_name.0.to_string();
        events::emit(&RuntimeEvent::ChildStarted {
            call: call.clone(),
            tool: tool.clone(),
            summary: summarize_input(&request.input),
            at: events::now_millis(),
        });
        let started = Instant::now();
        let outcome = self.0.invoke(request, context).await;
        let (ok, summary) = match &outcome {
            Ok(result) => (
                !result.result.is_error,
                summarize_output(&output_value(&result.result.output)),
            ),
            Err(error) => (false, summarize_output(&json!(error.to_string()))),
        };
        events::emit(&RuntimeEvent::ChildFinished {
            call,
            tool,
            ok,
            summary,
            millis: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        });
        outcome
    }
}

fn output_value(output: &ToolOutput) -> Value {
    match output {
        ToolOutput::Text(text) => json!(text),
        ToolOutput::Structured(value) => value.clone(),
        ToolOutput::Parts(parts) => json!(format!("{} parts", parts.len())),
        ToolOutput::Files(files) => json!(format!("{} files", files.len())),
    }
}
