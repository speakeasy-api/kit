//! Transparent lifecycle reporting for the hidden tools behind `compose`.
//! Effects observations are independent of the opt-in stderr display channel.

use std::{sync::Arc, time::Instant};

use agentkit_core::ToolOutput;
use agentkit_tools_core::{
    PermissionRequest, Tool, ToolContext, ToolError, ToolExecutionOutcome, ToolRequest, ToolResult,
    ToolSpec,
};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{
    effects::Observations,
    events::{self, RuntimeEvent, summarize_input, summarize_output},
};

/// Wraps a tool so its calls appear on the runtime side channel.
pub struct Observed<T> {
    tool: T,
    observations: Option<Observations>,
}

impl<T: Tool> Observed<T> {
    pub const fn new(tool: T) -> Self {
        Self {
            tool,
            observations: None,
        }
    }

    pub(crate) fn with_observations(mut self, observations: Observations) -> Self {
        self.observations = Some(observations);
        self
    }

    fn start(&self, request: &ToolRequest) -> Option<DisplayInvocation> {
        if let Some(observations) = &self.observations {
            observations.invocation_started();
        }
        DisplayInvocation::start(request)
    }

    fn finish(&self, display: Option<DisplayInvocation>, result: Result<&ToolResult, &ToolError>) {
        if let Some(observations) = &self.observations {
            observations.invocation_completed();
        }
        if let Some(display) = display {
            display.finish(result);
        }
    }
}

/// Wraps a dynamically dispatched tool without hiding specs or native outcomes.
pub(crate) fn shared(tool: Arc<dyn Tool>, observations: Observations) -> impl Tool {
    Observed::new(SharedTool(tool)).with_observations(observations)
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
        self.tool.spec()
    }
    fn current_spec(&self) -> Option<ToolSpec> {
        self.tool.current_spec()
    }
    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        self.tool.proposed_requests(request)
    }
    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let display = self.start(&request);
        let outcome = self.tool.invoke(request, context).await;
        self.finish(display, outcome.as_ref());
        outcome
    }
    async fn invoke_outcome(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        let display = self.start(&request);
        let outcome = self.tool.invoke_outcome(request, context).await;
        match &outcome {
            ToolExecutionOutcome::Completed(result) => self.finish(display, Ok(result)),
            ToolExecutionOutcome::Failed(error)
            | ToolExecutionOutcome::FailedBeforeInvocation(error) => {
                self.finish(display, Err(error))
            }
            // Neither interruption nor dropping an in-flight future is completion.
            ToolExecutionOutcome::Interrupted(_) => {}
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

    #[derive(Clone, Copy)]
    enum Mode {
        Complete,
        Pending,
        Failed,
        Cancelled,
        Interrupted,
    }
    struct Fixture {
        spec: ToolSpec,
        mode: Mode,
    }
    impl Fixture {
        fn new(mode: Mode) -> Self {
            Self {
                spec: ToolSpec::new(ToolName::new("fixture"), "fixture", json!({})),
                mode,
            }
        }
    }
    #[async_trait]
    impl Tool for Fixture {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        async fn invoke(
            &self,
            request: ToolRequest,
            _: &mut ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            match self.mode {
                Mode::Pending => std::future::pending().await,
                Mode::Complete => Ok(ToolResult::new(ToolResultPart::success(
                    request.call_id,
                    ToolOutput::text("private result"),
                ))),
                Mode::Failed => Err(ToolError::ExecutionFailed("failed".into())),
                Mode::Cancelled => Err(ToolError::Cancelled),
                Mode::Interrupted => panic!("native outcome must not use invoke fallback"),
            }
        }
        async fn invoke_outcome(
            &self,
            request: ToolRequest,
            context: &mut ToolContext<'_>,
        ) -> ToolExecutionOutcome {
            if matches!(self.mode, Mode::Interrupted) {
                return ToolExecutionOutcome::Interrupted(ToolInterruption::ApprovalRequired(
                    ApprovalRequest::new(
                        "approval",
                        "fixture",
                        ApprovalReason::PolicyRequiresConfirmation,
                        "approval",
                    ),
                ));
            }
            match self.invoke(request, context).await {
                Ok(result) => ToolExecutionOutcome::Completed(result),
                Err(error) => ToolExecutionOutcome::Failed(error),
            }
        }
    }
    fn context() -> OwnedToolContext {
        OwnedToolContext {
            session_id: SessionId::new("session"),
            turn_id: TurnId::new("turn"),
            metadata: MetadataMap::new(),
            permissions: Arc::new(AllowAllPermissions),
            resources: Arc::new(()),
            cancellation: None,
            execution_scope: None,
            approved_request: None,
        }
    }
    fn request() -> ToolRequest {
        ToolRequest::new(
            ToolCallId::new("private-call"),
            ToolName::new("fixture"),
            json!({"secret": "private arguments"}),
            SessionId::new("session"),
            TurnId::new("turn"),
        )
    }

    #[tokio::test]
    async fn observations_do_not_depend_on_display_events() {
        if crate::effects::isolated_test(
            "tools::observed::tests::observations_do_not_depend_on_display_events",
        ) {
            return;
        }
        assert!(!events::enabled());
        let observations = Observations::local_session();
        let tool =
            Observed::new(Fixture::new(Mode::Complete)).with_observations(observations.clone());
        tool.invoke(request(), &mut context().borrowed())
            .await
            .unwrap();
        assert!(observations.snapshot().tool_execution_start_reported);
        assert!(observations.snapshot().tool_execution_completion_reported);
        assert!(
            !serde_json::to_string(&observations.snapshot())
                .unwrap()
                .contains("private")
        );
    }

    #[tokio::test]
    async fn dropping_running_invocation_does_not_report_completion() {
        let observations = Observations::local_session();
        let tool =
            Observed::new(Fixture::new(Mode::Pending)).with_observations(observations.clone());
        let owned = context();
        let mut context = owned.borrowed();
        let mut invocation = Box::pin(tool.invoke(request(), &mut context));
        tokio::select! { biased; _ = &mut invocation => panic!("pending"), () = tokio::task::yield_now() => {} }
        drop(invocation);
        assert!(observations.snapshot().tool_execution_start_reported);
        assert!(!observations.snapshot().tool_execution_completion_reported);
        assert!(observations.snapshot().observation_incomplete);
    }

    #[tokio::test]
    async fn shared_wrapper_preserves_native_interruption_failure_and_cancellation() {
        for mode in [
            Mode::Complete,
            Mode::Failed,
            Mode::Cancelled,
            Mode::Interrupted,
        ] {
            let observations = Observations::local_session();
            let tool = shared(Arc::new(Fixture::new(mode)), observations.clone());
            let outcome = tool
                .invoke_outcome(request(), &mut context().borrowed())
                .await;
            match mode {
                Mode::Complete => assert!(matches!(outcome, ToolExecutionOutcome::Completed(_))),
                Mode::Failed => assert!(matches!(
                    outcome,
                    ToolExecutionOutcome::Failed(ToolError::ExecutionFailed(_))
                )),
                Mode::Cancelled => assert!(matches!(
                    outcome,
                    ToolExecutionOutcome::Failed(ToolError::Cancelled)
                )),
                Mode::Interrupted => {
                    assert!(matches!(outcome, ToolExecutionOutcome::Interrupted(_)))
                }
                Mode::Pending => unreachable!(),
            }
            assert!(observations.snapshot().tool_execution_start_reported);
            assert_eq!(
                observations.snapshot().tool_execution_completion_reported,
                !matches!(mode, Mode::Interrupted)
            );
        }
    }
}
