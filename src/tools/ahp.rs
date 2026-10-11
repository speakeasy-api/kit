//! Awaited tool boundaries. The host owns SDK reduction and transport; this module
//! only carries effective values and immutable, source-scoped invocation lineage.

use std::sync::Arc;

use agentkit_core::TurnCancellation;
use agentkit_tools_core::{
    ApprovalRequest, PermissionRequest, Tool, ToolCatalogEvent, ToolContext, ToolError,
    ToolExecutionOutcome, ToolExecutor, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

const PARENT: &str = "kit.ahp.tool_parent";

/// An event identity, never a provider tool-call identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEventIdentity {
    pub source: String,
    pub id: String,
}

/// The SDK host can short-circuit an invocation without duplicating its policy here.
pub enum ToolBeforeOutcome {
    Invoke {
        request: ToolRequest,
        event: ToolEventIdentity,
    },
    Complete(ToolExecutionOutcome),
}

/// Host mapping for the SDK's awaited, Send-safe tool boundaries.
///
/// Implementations must apply SDK input/output effects and preserve the original
/// call/session/turn identities. The returned event is the effective before event.
/// Cancellation is forwarded from the real execution context. The host decides
/// how to bound admission and terminal delivery; an already-cancelled token must
/// not suppress required terminal notifications automatically.
#[async_trait]
pub trait ToolHooks: Send + Sync {
    async fn before(
        &self,
        request: ToolRequest,
        parent: Option<ToolEventIdentity>,
        cancellation: Option<TurnCancellation>,
    ) -> ToolBeforeOutcome;

    async fn after(
        &self,
        request: &ToolRequest,
        event: ToolEventIdentity,
        outcome: ToolExecutionOutcome,
        cancellation: Option<TurnCancellation>,
    ) -> ToolExecutionOutcome;
}

/// Wrap the native executor once. Scoped child executors retain their owning
/// before-event identity even when compose moves work to a background task.
pub struct AhpToolExecutor {
    inner: Arc<dyn ToolExecutor>,
    hooks: Arc<dyn ToolHooks>,
    parent: Option<ToolEventIdentity>,
}

impl AhpToolExecutor {
    pub fn new(inner: Arc<dyn ToolExecutor>, hooks: Arc<dyn ToolHooks>) -> Self {
        Self {
            inner,
            hooks,
            parent: None,
        }
    }

    fn scoped(&self, parent: ToolEventIdentity) -> Self {
        Self {
            inner: self.inner.clone(),
            hooks: self.hooks.clone(),
            parent: Some(parent),
        }
    }

    /// Rebind a private dynamic executor without losing its invocation parent.
    pub(crate) fn for_context(
        inner: Arc<dyn ToolExecutor>,
        hooks: Arc<dyn ToolHooks>,
        context: &ToolContext<'_>,
    ) -> Self {
        Self {
            inner,
            hooks,
            parent: parent(context),
        }
    }

    async fn run(
        &self,
        request: ToolRequest,
        approval: Option<&ApprovalRequest>,
        context: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        let (request, event) = match self
            .hooks
            .before(request, self.parent.clone(), context.cancellation.clone())
            .await
        {
            ToolBeforeOutcome::Invoke { request, event } => (request, event),
            ToolBeforeOutcome::Complete(outcome) => return outcome,
        };
        let mut metadata = context.capability.metadata.clone();
        // This metadata is local to the borrowed context, not the request or
        // transcript. Children inherit the immutable executor, not a task-local.
        let identity = match serde_json::to_value(&event) {
            Ok(identity) => identity,
            Err(error) => {
                return self
                    .hooks
                    .after(
                        &request,
                        event,
                        ToolExecutionOutcome::FailedBeforeInvocation(ToolError::ExecutionFailed(
                            format!("cannot serialize AHP tool event identity: {error}"),
                        )),
                        context.cancellation.clone(),
                    )
                    .await;
            }
        };
        metadata.insert(PARENT.into(), identity);
        let mut capability = context.capability.clone();
        capability.metadata = &metadata;
        let mut scope = context.execution_scope.clone();
        if let Some(scope) = &mut scope {
            scope.executor = Arc::new(self.scoped(event.clone()));
        }
        let mut nested = ToolContext {
            capability,
            permissions: context.permissions,
            resources: context.resources,
            cancellation: context.cancellation.clone(),
            execution_scope: scope,
            approved_request: context.approved_request.clone(),
        };
        let outcome = match approval {
            Some(approval) => {
                self.inner
                    .execute_approved(request.clone(), approval, &mut nested)
                    .await
            }
            None => self.inner.execute(request.clone(), &mut nested).await,
        };
        self.hooks
            .after(&request, event, outcome, context.cancellation.clone())
            .await
    }
}

fn parent(context: &ToolContext<'_>) -> Option<ToolEventIdentity> {
    context
        .capability
        .metadata
        .get(PARENT)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

#[async_trait]
impl ToolExecutor for AhpToolExecutor {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner.specs()
    }
    fn drain_catalog_events(&self) -> Vec<ToolCatalogEvent> {
        self.inner.drain_catalog_events()
    }
    async fn execute(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        self.run(request, None, context).await
    }
    async fn execute_approved(
        &self,
        request: ToolRequest,
        approval: &ApprovalRequest,
        context: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        self.run(request, Some(approval), context).await
    }
}

/// Boundary for tools invoked directly rather than through the wrapped executor.
/// Do not install this on a tool already owned by `AhpToolExecutor`.
pub struct AhpTool<T> {
    inner: T,
    hooks: Arc<dyn ToolHooks>,
}

impl<T> AhpTool<T> {
    pub fn new(inner: T, hooks: Arc<dyn ToolHooks>) -> Self {
        Self { inner, hooks }
    }
}

impl<T: Tool> AhpTool<T> {
    async fn run(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
        direct: bool,
    ) -> ToolExecutionOutcome {
        let (request, event) = match self
            .hooks
            .before(request, parent(context), context.cancellation.clone())
            .await
        {
            ToolBeforeOutcome::Invoke { request, event } => (request, event),
            ToolBeforeOutcome::Complete(outcome) => return outcome,
        };
        let mut metadata = context.capability.metadata.clone();
        let identity = match serde_json::to_value(&event) {
            Ok(identity) => identity,
            Err(error) => {
                return self
                    .hooks
                    .after(
                        &request,
                        event,
                        ToolExecutionOutcome::FailedBeforeInvocation(ToolError::ExecutionFailed(
                            format!("cannot serialize AHP tool event identity: {error}"),
                        )),
                        context.cancellation.clone(),
                    )
                    .await;
            }
        };
        metadata.insert(PARENT.into(), identity);
        let mut capability = context.capability.clone();
        capability.metadata = &metadata;
        let mut scope = context.execution_scope.clone();
        if let Some(scope) = &mut scope {
            scope.executor = Arc::new(AhpToolExecutor {
                inner: scope.executor.clone(),
                hooks: self.hooks.clone(),
                parent: Some(event.clone()),
            });
        }
        let mut nested = ToolContext {
            capability,
            permissions: context.permissions,
            resources: context.resources,
            cancellation: context.cancellation.clone(),
            execution_scope: scope,
            approved_request: context.approved_request.clone(),
        };
        let outcome = if direct {
            match self.inner.invoke(request.clone(), &mut nested).await {
                Ok(result) => ToolExecutionOutcome::Completed(result),
                Err(error) => ToolExecutionOutcome::Failed(error),
            }
        } else {
            self.inner
                .invoke_outcome(request.clone(), &mut nested)
                .await
        };
        self.hooks
            .after(&request, event, outcome, context.cancellation.clone())
            .await
    }
}

#[async_trait]
impl<T: Tool> Tool for AhpTool<T> {
    fn spec(&self) -> &ToolSpec {
        self.inner.spec()
    }
    fn current_spec(&self) -> Option<ToolSpec> {
        self.inner.current_spec()
    }
    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        self.inner.proposed_requests(request)
    }
    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        match self.run(request, context, true).await {
            ToolExecutionOutcome::Completed(result) => Ok(result),
            ToolExecutionOutcome::FailedBeforeInvocation(error)
            | ToolExecutionOutcome::Failed(error) => Err(error),
            ToolExecutionOutcome::Interrupted(_) => {
                Err(ToolError::Unavailable("tool requires approval".into()))
            }
        }
    }
    async fn invoke_outcome(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        self.run(request, context, false).await
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    clippy::disallowed_macros,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]
mod tests {
    use super::*;
    use agentkit_core::{MetadataMap, SessionId, ToolOutput, ToolResultPart, TurnId};
    use agentkit_tools_core::ToolName;
    use agentkit_tools_core::{
        AllowAllPermissions, BasicToolExecutor, OwnedToolContext, ToolExecutionScope, ToolRegistry,
    };
    use serde_json::json;

    struct Hooks;
    #[async_trait]
    impl ToolHooks for Hooks {
        async fn before(
            &self,
            mut request: ToolRequest,
            parent: Option<ToolEventIdentity>,
            _: Option<TurnCancellation>,
        ) -> ToolBeforeOutcome {
            request.input["parent"] = json!(parent);
            let event = ToolEventIdentity {
                source: "kit-test".into(),
                id: format!("before-{}", request.call_id.0),
            };
            ToolBeforeOutcome::Invoke { request, event }
        }
        async fn after(
            &self,
            _: &ToolRequest,
            _: ToolEventIdentity,
            outcome: ToolExecutionOutcome,
            _: Option<TurnCancellation>,
        ) -> ToolExecutionOutcome {
            match outcome {
                ToolExecutionOutcome::Completed(mut result) => {
                    if let ToolOutput::Text(text) = &mut result.result.output {
                        text.push_str("-after");
                    }
                    ToolExecutionOutcome::Completed(result)
                }
                outcome => outcome,
            }
        }
    }

    struct ChildTool(ToolSpec);
    #[async_trait]
    impl Tool for ChildTool {
        fn spec(&self) -> &ToolSpec {
            &self.0
        }
        async fn invoke(
            &self,
            request: ToolRequest,
            context: &mut ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            if request.input["spawn"] == true {
                let scope = context.execution_scope.clone().unwrap();
                let child = ToolRequest::new(
                    "child",
                    "child",
                    json!({}),
                    request.session_id,
                    request.turn_id,
                );
                let outcome = tokio::spawn(async move { scope.execute_child(child).await })
                    .await
                    .unwrap();
                return match outcome {
                    ToolExecutionOutcome::Completed(result) => Ok(result),
                    _ => panic!("unexpected child outcome"),
                };
            }
            Ok(ToolResult::new(ToolResultPart::success(
                request.call_id,
                ToolOutput::text(request.input["parent"].to_string()),
            )))
        }
    }

    fn setup() -> (Arc<dyn ToolExecutor>, OwnedToolContext) {
        let inner = Arc::new(BasicToolExecutor::from_registry(ToolRegistry::new().with(
            ChildTool(ToolSpec::new(ToolName::new("child"), "child", json!({}))),
        )));
        let executor: Arc<dyn ToolExecutor> =
            Arc::new(AhpToolExecutor::new(inner, Arc::new(Hooks)));
        let scope = ToolExecutionScope {
            executor: executor.clone(),
            session_id: SessionId::new("s"),
            turn_id: TurnId::new("t"),
            permissions: Arc::new(AllowAllPermissions),
            resources: Arc::new(()),
            cancellation: None,
        };
        let context = scope.nested_context(MetadataMap::new());
        (executor, context)
    }

    #[tokio::test]
    async fn concurrent_owned_children_keep_distinct_parents_and_effective_values() {
        let (executor, context) = setup();
        let a = ToolRequest::new("a", "child", json!({"spawn":true}), "s", "t");
        let b = ToolRequest::new("b", "child", json!({"spawn":true}), "s", "t");
        let (a, b) = tokio::join!(
            executor.execute_owned(a, context.clone()),
            executor.execute_owned(b, context)
        );
        for (outcome, parent) in [(a, "before-a"), (b, "before-b")] {
            let ToolExecutionOutcome::Completed(result) = outcome else {
                panic!("unexpected outcome")
            };
            assert_eq!(
                result.result.output,
                ToolOutput::text(format!(
                    "{}-after-after",
                    json!({"source":"kit-test","id":parent})
                ))
            );
        }
    }

    #[tokio::test]
    async fn direct_invoke_and_outcome_apply_the_same_boundary() {
        let (_, context) = setup();
        let tool = AhpTool::new(
            ChildTool(ToolSpec::new(ToolName::new("child"), "child", json!({}))),
            Arc::new(Hooks),
        );
        let request = ToolRequest::new("direct", "child", json!({}), "s", "t");
        let direct = tool
            .invoke(request.clone(), &mut context.borrowed())
            .await
            .unwrap();
        let ToolExecutionOutcome::Completed(outcome) =
            tool.invoke_outcome(request, &mut context.borrowed()).await
        else {
            panic!("unexpected outcome")
        };
        assert_eq!(direct.result.output, ToolOutput::text("null-after"));
        assert_eq!(outcome.result.output, direct.result.output);
    }
}
