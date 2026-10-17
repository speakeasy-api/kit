use std::sync::{Arc, Mutex};

use agenthooksprotocol::client::{HookError, Permission};
use agenthooksprotocol::contracts::Contract;
use agenthooksprotocol::generated::Presence;
use agenthooksprotocol::inputs::{MessageInput, ToolAfterInput, ToolBeforeInput};
use agenthooksprotocol::{
    Attachment, CanonicalMessageRole, EventType, ExecutionEventError, ExecutionEventExecution,
    ExecutionEventExecutionExecuted, ExecutionEventExecutionSkippedCancelled,
    ExecutionEventExecutionSkippedOther, ExecutionEventExecutionSkippedPolicy,
    ExecutionEventExecutionSkippedSuppliedResult, ToolAfterInputOrigin, ToolAfterInputOutcome,
    ToolBeforeInputOrigin, state,
};
use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    PermissionCode, PermissionDecision, PermissionDenial, PermissionRequest, Tool, ToolContext,
    ToolError, ToolExecutionOutcome, ToolInterruption, ToolRegistry, ToolRequest, ToolResult,
    ToolSpec,
};
use async_trait::async_trait;
use serde_json::Value;

use super::Hooks;

/// Wraps every tool so `tool.before` and `tool.after` surround its invocation.
pub(crate) fn hooked_tools(registry: ToolRegistry, hooks: &Arc<Hooks>) -> ToolRegistry {
    registry
        .tools()
        .into_iter()
        .fold(ToolRegistry::new(), |mut hooked, tool| {
            hooked.register(HookedTool {
                tool,
                hooks: Arc::clone(hooks),
                validator: Mutex::default(),
            });
            hooked
        })
}

struct HookedTool {
    tool: Arc<dyn Tool>,
    hooks: Arc<Hooks>,
    validator: Mutex<Option<(Value, Arc<jsonschema::Validator>)>>,
}

enum Before {
    Run(ToolRequest),
    Supplied(ToolRequest, ToolExecutionOutcome),
    Return(ToolExecutionOutcome),
}

impl HookedTool {
    async fn before(&self, mut request: ToolRequest, context: &ToolContext<'_>) -> Before {
        let validator = match self.validator() {
            Ok(validator) => validator,
            Err(error) => return Before::Return(denied(error)),
        };
        let mut input = ToolBeforeInput::new(
            request.call_id.to_string(),
            request.tool_name.to_string(),
            request.input.clone(),
            request.tool_name.to_string(),
            ToolBeforeInputOrigin::Native,
        );
        input.session = Some(self.hooks.session(&request.session_id.to_string()));
        input.turn = Some(agenthooksprotocol::ToolBeforeInputTurn::new(
            request.turn_id.to_string(),
        ));
        let boundary = match self
            .hooks
            .sdk
            .tool_before(input)
            .arguments_contract(Contract::<Value>::validated(move |input| {
                validator.validate(input).map_err(|error| error.to_string())
            }))
            .initial_snapshot(state::initial(Permission::Allow))
        {
            Ok(boundary) => boundary,
            Err(error) => return Before::Return(denied(error.to_string())),
        };
        let result = match &context.cancellation {
            Some(cancellation) => boundary.cancel_when(cancellation.cancelled()).await,
            None => boundary.await,
        };
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::warn!(event = "tool.before", %error, "agent hook dispatch failed");
                return Before::Return(denied(error.to_string()));
            }
        };
        super::report(EventType::ToolBefore, &outcome.diagnostics);
        if outcome.outcome.is_denied() {
            return Before::Return(denied(super::denial(&outcome.outcome)));
        }
        if let Some(value) = outcome.outcome.supplied_result() {
            let output = match value {
                Value::String(text) => ToolOutput::text(text.clone()),
                value => ToolOutput::structured(value.clone()),
            };
            let result = ToolResultPart::success(request.call_id.clone(), output);
            return Before::Supplied(
                request,
                ToolExecutionOutcome::Completed(ToolResult::new(result)),
            );
        }
        let reauthorize = !outcome.outcome.can_execute();
        if reauthorize
            && !(outcome.outcome.approval_invalidated && outcome.outcome.requires_approval())
        {
            return Before::Return(denied("agent hook did not authorize the call".into()));
        }
        request.input = outcome.input;
        if reauthorize && let Some(outcome) = self.authorize(&request, context) {
            return Before::Return(outcome);
        }
        Before::Run(request)
    }

    fn validator(&self) -> Result<Arc<jsonschema::Validator>, String> {
        let mut cached = self
            .validator
            .lock()
            .map_err(|_| "tool schema cache is poisoned".to_owned())?;
        let schema = match self.tool.current_spec() {
            Some(spec) => spec.input_schema,
            None => self.tool.spec().input_schema.clone(),
        };
        if let Some((cached_schema, validator)) = cached.as_ref()
            && *cached_schema == schema
        {
            return Ok(Arc::clone(validator));
        }
        let validator = Arc::new(
            jsonschema::validator_for(&schema)
                .map_err(|error| format!("invalid tool schema: {error}"))?,
        );
        *cached = Some((schema, Arc::clone(&validator)));
        Ok(validator)
    }

    /// Applies the host permission policy to input a hook edited; `None`
    /// authorizes the call.
    fn authorize(
        &self,
        request: &ToolRequest,
        context: &ToolContext<'_>,
    ) -> Option<ToolExecutionOutcome> {
        let requests = match self.tool.proposed_requests(request) {
            Ok(requests) => requests,
            Err(error) => return Some(ToolExecutionOutcome::FailedBeforeInvocation(error)),
        };
        for permission in requests {
            match context.permissions.evaluate(permission.as_ref()) {
                PermissionDecision::Allow => {}
                PermissionDecision::Deny(denial) => {
                    return Some(ToolExecutionOutcome::FailedBeforeInvocation(
                        ToolError::PermissionDenied(denial),
                    ));
                }
                PermissionDecision::RequireApproval(mut approval) => {
                    let approved = context
                        .approved_request
                        .as_ref()
                        .is_some_and(|approved| approved.id == approval.id);
                    if !approved {
                        approval.call_id = Some(request.call_id.clone());
                        return Some(ToolExecutionOutcome::Interrupted(
                            ToolInterruption::ApprovalRequired(approval),
                        ));
                    }
                }
            }
        }
        None
    }

    async fn after(&self, request: &ToolRequest, outcome: &ToolExecutionOutcome, supplied: bool) {
        let execution = match outcome {
            _ if supplied => ExecutionEventExecution::SkippedSuppliedResult(
                ExecutionEventExecutionSkippedSuppliedResult::new(),
            ),
            ToolExecutionOutcome::FailedBeforeInvocation(ToolError::Cancelled) => {
                ExecutionEventExecution::SkippedCancelled(
                    ExecutionEventExecutionSkippedCancelled::new(),
                )
            }
            ToolExecutionOutcome::FailedBeforeInvocation(ToolError::PermissionDenied(_)) => {
                ExecutionEventExecution::SkippedPolicy(ExecutionEventExecutionSkippedPolicy::new())
            }
            ToolExecutionOutcome::FailedBeforeInvocation(error) => {
                let mut skipped = ExecutionEventExecutionSkippedOther::new();
                skipped.detail = Presence::Present(error.to_string());
                ExecutionEventExecution::SkippedOther(skipped)
            }
            _ => ExecutionEventExecution::Executed(ExecutionEventExecutionExecuted::new()),
        };
        let (status, output, error) = match outcome {
            ToolExecutionOutcome::Completed(result) if result.result.is_error => {
                (ToolAfterInputOutcome::Error, Some(&result.result), None)
            }
            ToolExecutionOutcome::Completed(result) => {
                (ToolAfterInputOutcome::Ok, Some(&result.result), None)
            }
            ToolExecutionOutcome::Failed(ToolError::Cancelled)
            | ToolExecutionOutcome::FailedBeforeInvocation(ToolError::Cancelled) => {
                (ToolAfterInputOutcome::Cancelled, None, None)
            }
            ToolExecutionOutcome::Failed(error)
            | ToolExecutionOutcome::FailedBeforeInvocation(error) => {
                (ToolAfterInputOutcome::Error, None, Some(error.to_string()))
            }
            ToolExecutionOutcome::Interrupted(_) => return,
        };
        let mut input = ToolAfterInput::new(
            request.call_id.to_string(),
            Box::new(execution),
            Vec::new(),
            status,
            request.tool_name.to_string(),
            request.input.clone(),
            request.tool_name.to_string(),
            ToolAfterInputOrigin::Native,
        );
        input.session = Some(self.hooks.session(&request.session_id.to_string()));
        input.turn = Some(agenthooksprotocol::ToolAfterInputTurn::new(
            request.turn_id.to_string(),
        ));
        if let ToolExecutionOutcome::Completed(result) = outcome {
            input.duration_ms = result
                .duration
                .and_then(|duration| serde_json::Number::from_f64(duration.as_secs_f64() * 1000.0));
        }
        input.error = error.map(|message| Box::new(ExecutionEventError::new("tool", message)));
        let items = output
            .map(|result| {
                vec![MessageInput::synthesized(
                    format!("{}/output", request.call_id),
                    CanonicalMessageRole::Tool,
                    super::content_parts(
                        &format!("{}/output/0", request.call_id),
                        &agentkit_core::Part::ToolResult(result.clone()),
                    ),
                )]
            })
            .unwrap_or_default();
        let result = match input
            .with_sources::<Attachment>()
            .with_items(items)
            .into_host_event()
        {
            Ok((event, _)) => self.hooks.sdk.event_for(EventType::ToolAfter, event).await,
            Err(error) => Err(HookError::new(error.to_string())),
        };
        super::settle(EventType::ToolAfter, result);
    }
}

fn denied(message: String) -> ToolExecutionOutcome {
    ToolExecutionOutcome::FailedBeforeInvocation(ToolError::PermissionDenied(PermissionDenial {
        code: PermissionCode::CustomPolicyDenied,
        message,
        metadata: Default::default(),
    }))
}

#[async_trait]
impl Tool for HookedTool {
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
        match self.invoke_outcome(request, context).await {
            ToolExecutionOutcome::Completed(result) => Ok(result),
            ToolExecutionOutcome::Failed(error)
            | ToolExecutionOutcome::FailedBeforeInvocation(error) => Err(error),
            ToolExecutionOutcome::Interrupted(_) => Err(ToolError::Internal(
                "tool interruption requires invoke_outcome".into(),
            )),
        }
    }

    async fn invoke_outcome(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        let request = match self.before(request, context).await {
            Before::Run(request) => request,
            Before::Supplied(request, outcome) => {
                self.after(&request, &outcome, true).await;
                return outcome;
            }
            Before::Return(outcome) => return outcome,
        };
        let outcome = self.tool.invoke_outcome(request.clone(), context).await;
        self.after(&request, &outcome, false).await;
        outcome
    }
}
