//! Transparent hidden-tool wrapper with canonical ACP lifecycle projection.

use std::sync::Arc;

use agentkit_tools_core::{
    PermissionRequest, Tool, ToolContext, ToolError, ToolExecutionOutcome, ToolRequest, ToolResult,
    ToolSpec,
};
use async_trait::async_trait;

/// Wraps a tool so its calls appear in canonical ACP notifications.
pub struct Observed<T>(T, Option<std::path::PathBuf>);

impl<T: Tool> Observed<T> {
    pub const fn new(tool: T) -> Self {
        Self(tool, None)
    }

    pub(crate) fn with_root(mut self, root: std::path::PathBuf) -> Self {
        self.1 = Some(root);
        self
    }
}

/// Wraps a dynamically dispatched tool without hiding its changing spec.
pub(crate) fn shared(tool: Arc<dyn Tool>) -> impl Tool {
    Observed(SharedTool(tool), None)
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
        let projection =
            crate::protocols::acp::tool_projection::Invocation::start(&request, self.1.as_deref());
        let outcome = self.0.invoke(request, context).await;
        if let Some(projection) = projection {
            projection.finish(outcome.as_ref().is_ok_and(|result| !result.result.is_error));
        }
        outcome
    }

    async fn invoke_outcome(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        let projection =
            crate::protocols::acp::tool_projection::Invocation::start(&request, self.1.as_deref());
        let outcome = self.0.invoke_outcome(request, context).await;
        if let Some(projection) = projection {
            projection.finish(matches!(&outcome, ToolExecutionOutcome::Completed(result) if !result.result.is_error));
        }
        outcome
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use agentkit_core::{MetadataMap, SessionId, ToolCallId, ToolOutput, ToolResultPart, TurnId};
    use agentkit_tools_core::{
        AllowAllPermissions, ApprovalReason, ApprovalRequest, OwnedToolContext, ToolInterruption,
        ToolName,
    };
    use serde_json::json;

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
    async fn completion_with_stalled_stderr() {
        if !crate::events::test_support::with_stalled_stderr(
            "tools::observed::tests::completion_with_stalled_stderr",
        ) {
            return;
        }
        preserve_native_outcomes().await;
        let tool = Observed::new(DirectTool {
            spec: ToolSpec::new(ToolName::new("direct"), "direct", json!({})),
        });
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
            ToolName::new("direct"),
            json!({}),
            context.session_id.clone(),
            context.turn_id.clone(),
        );
        let result = tool.invoke(request, &mut context.borrowed()).await.unwrap();
        assert_eq!(result.result.output, ToolOutput::text("done"));
    }

    struct DirectTool {
        spec: ToolSpec,
    }

    #[async_trait]
    impl Tool for DirectTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        async fn invoke(
            &self,
            request: ToolRequest,
            _: &mut ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::new(ToolResultPart::success(
                request.call_id,
                ToolOutput::text("done"),
            )))
        }
    }

    #[tokio::test]
    async fn both_wrappers_preserve_native_outcomes() {
        preserve_native_outcomes().await;
    }

    async fn preserve_native_outcomes() {
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
