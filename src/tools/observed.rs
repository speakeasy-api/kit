//! Lifecycle reporting for the hidden tools behind `compose`.
//!
//! The wrapper is transparent to the model and to compose: it forwards the
//! spec, permission requests, and invocation untouched, and only publishes
//! start/finish events on the runtime side channel (see [`crate::events`]) so
//! a client can draw what a Runlet program is doing while it runs.

use std::{sync::Arc, time::Instant};

use agentkit_core::ToolOutput;
use agentkit_tools_core::{
    PermissionRequest, Tool, ToolContext, ToolError, ToolExecutionOutcome, ToolRequest, ToolResult,
    ToolSpec,
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

/// Wraps a dynamically dispatched tool without hiding its changing spec.
pub(crate) fn shared(tool: Arc<dyn Tool>) -> impl Tool {
    Observed(SharedTool(tool))
}

struct SharedTool(Arc<dyn Tool>);

#[async_trait]
impl Tool for SharedTool {
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
        self.0.invoke(request, context).await
    }

    async fn invoke_outcome(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        self.0.invoke_outcome(request, context).await
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
        let display = DisplayInvocation::start(&request);
        let outcome = self.0.invoke(request, context).await;
        if let Some(display) = display {
            display.finish(outcome.as_ref());
        }
        outcome
    }

    async fn invoke_outcome(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        let display = DisplayInvocation::start(&request);
        let outcome = self.0.invoke_outcome(request, context).await;
        if let Some(display) = display {
            match &outcome {
                ToolExecutionOutcome::Completed(result) => display.finish(Ok(result)),
                ToolExecutionOutcome::Failed(error)
                | ToolExecutionOutcome::FailedBeforeInvocation(error) => display.finish(Err(error)),
                // An approval interruption is not a completed invocation.
                ToolExecutionOutcome::Interrupted(_) => {}
            }
        }
        outcome
    }
}

struct DisplayInvocation {
    call: String,
    tool: String,
    started: Instant,
}

impl DisplayInvocation {
    fn start(request: &ToolRequest) -> Option<Self> {
        if !events::enabled() {
            return None;
        }
        let call = request.call_id.0.clone();
        let tool = request.tool_name.0.to_string();
        events::emit(&RuntimeEvent::ChildStarted {
            call: call.clone(),
            tool: tool.clone(),
            summary: summarize_input(&request.input),
            at: events::now_millis(),
        });
        Some(Self {
            call,
            tool,
            started: Instant::now(),
        })
    }

    fn finish(self, result: Result<&ToolResult, &ToolError>) {
        let (ok, summary) = match result {
            Ok(result) => (
                !result.result.is_error,
                summarize_output(&output_value(&result.result.output)),
            ),
            Err(error) => (false, summarize_output(&json!(error.to_string()))),
        };
        events::emit(&RuntimeEvent::ChildFinished {
            call: self.call,
            tool: self.tool,
            ok,
            summary,
            millis: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{MetadataMap, SessionId, ToolCallId, ToolResultPart, TurnId};
    use agentkit_tools_core::{
        AllowAllPermissions, ApprovalReason, ApprovalRequest, OwnedToolContext, ToolInterruption,
        ToolName,
    };

    #[derive(Clone, Copy, Debug)]
    enum Mode {
        Completed,
        Failed,
        FailedBeforeInvocation,
        Cancelled,
        Interrupted,
    }

    struct NativeTool {
        spec: ToolSpec,
        mode: Mode,
    }

    #[async_trait]
    impl Tool for NativeTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        async fn invoke(
            &self,
            _: ToolRequest,
            _: &mut ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            panic!("wrapper must forward invoke_outcome, not use the invoke fallback")
        }

        async fn invoke_outcome(
            &self,
            request: ToolRequest,
            _: &mut ToolContext<'_>,
        ) -> ToolExecutionOutcome {
            match self.mode {
                Mode::Completed => ToolExecutionOutcome::Completed(ToolResult::new(
                    ToolResultPart::success(request.call_id, ToolOutput::text("done")),
                )),
                Mode::Failed => {
                    ToolExecutionOutcome::Failed(ToolError::ExecutionFailed("failed".into()))
                }
                Mode::FailedBeforeInvocation => ToolExecutionOutcome::FailedBeforeInvocation(
                    ToolError::Unavailable("not started".into()),
                ),
                Mode::Cancelled => ToolExecutionOutcome::Failed(ToolError::Cancelled),
                Mode::Interrupted => ToolExecutionOutcome::Interrupted(
                    ToolInterruption::ApprovalRequired(ApprovalRequest::new(
                        "approval",
                        "native",
                        ApprovalReason::PolicyRequiresConfirmation,
                        "approval",
                    )),
                ),
            }
        }
    }

    #[tokio::test]
    async fn both_wrappers_preserve_native_outcomes() {
        for mode in [
            Mode::Completed,
            Mode::Failed,
            Mode::FailedBeforeInvocation,
            Mode::Cancelled,
            Mode::Interrupted,
        ] {
            for dynamic in [false, true] {
                let native = NativeTool {
                    spec: ToolSpec::new(ToolName::new("native"), "native", json!({})),
                    mode,
                };
                let tool: Box<dyn Tool> = if dynamic {
                    Box::new(shared(Arc::new(native)))
                } else {
                    Box::new(Observed::new(native))
                };
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
                    ToolName::new("native"),
                    json!({}),
                    context.session_id.clone(),
                    context.turn_id.clone(),
                );
                let outcome = tool.invoke_outcome(request, &mut context.borrowed()).await;
                let preserved = match (mode, outcome) {
                    (Mode::Completed, ToolExecutionOutcome::Completed(result)) => {
                        result.result.output == ToolOutput::text("done")
                    }
                    (
                        Mode::Failed,
                        ToolExecutionOutcome::Failed(ToolError::ExecutionFailed(message)),
                    ) => message == "failed",
                    (
                        Mode::FailedBeforeInvocation,
                        ToolExecutionOutcome::FailedBeforeInvocation(ToolError::Unavailable(
                            message,
                        )),
                    ) => message == "not started",
                    (Mode::Cancelled, ToolExecutionOutcome::Failed(ToolError::Cancelled)) => true,
                    (Mode::Interrupted, ToolExecutionOutcome::Interrupted(_)) => true,
                    _ => false,
                };
                assert!(preserved, "{mode:?}, shared={dynamic}");
            }
        }
    }
}
