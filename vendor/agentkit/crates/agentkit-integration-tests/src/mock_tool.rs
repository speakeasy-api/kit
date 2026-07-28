//! Mock [`Tool`] implementations that record their own invocations.
//!
//! Three flavours:
//!
//! - [`RecordingTool`] — wraps any closure that maps a [`ToolRequest`] to a
//!   [`ToolOutput`]. Records every invocation for assertion. Use this when
//!   tests want to script tool behaviour and inspect what got called.
//! - [`StaticTool`] — even thinner: returns the same [`ToolOutput`] for
//!   every call. Records invocations.
//! - [`BlockingTool`] — invocation parks on a [`tokio::sync::Notify`]
//!   until the test calls [`BlockingTool::release`]. Combined with the
//!   `agentkit-task-manager` `Background` /
//!   `ForegroundThenDetachAfter` routing decisions, this lets tests
//!   exercise mid-turn vs post-turn delivery of deferred tool results.
//!
//! Plus [`NameRoutingPolicy`], a [`TaskRoutingPolicy`] keyed by tool name.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_task_manager::{RoutingDecision, TaskRoutingPolicy};
use agentkit_tools_core::{
    Tool, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Notify;

type Handler = dyn Fn(&ToolRequest) -> Result<ToolOutput, ToolError> + Send + Sync + 'static;

/// Mock tool with a closure-based handler.
///
/// Cheap to clone — shares the invocation log across clones.
#[derive(Clone)]
pub struct RecordingTool {
    spec: ToolSpec,
    invocations: Arc<Mutex<Vec<ToolRequest>>>,
    handler: Arc<Handler>,
}

impl RecordingTool {
    /// Build a tool with the given spec and handler. The handler is called
    /// for every invocation and its result is wrapped into a successful
    /// [`ToolResult`] (or an error if the closure returns one).
    pub fn new(
        spec: ToolSpec,
        handler: impl Fn(&ToolRequest) -> Result<ToolOutput, ToolError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            spec,
            invocations: Arc::new(Mutex::new(Vec::new())),
            handler: Arc::new(handler),
        }
    }

    /// Returns every recorded invocation, in call order.
    pub fn invocations(&self) -> Vec<ToolRequest> {
        self.invocations.lock().unwrap().clone()
    }

    /// Returns the number of recorded invocations.
    pub fn call_count(&self) -> usize {
        self.invocations.lock().unwrap().len()
    }
}

#[async_trait]
impl Tool for RecordingTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        self.invocations.lock().unwrap().push(request.clone());
        let output = (self.handler)(&request)?;
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            output,
        )))
    }
}

/// Static-output mock tool. Returns the same [`ToolOutput`] for every call.
#[derive(Clone)]
pub struct StaticTool {
    inner: RecordingTool,
}

impl StaticTool {
    /// Build a tool returning `output` (cloned) for every invocation. The
    /// input schema accepts anything.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        output: ToolOutput,
    ) -> Self {
        let spec = ToolSpec::new(
            ToolName::new(name),
            description,
            json!({ "type": "object", "additionalProperties": true }),
        );
        let cloneable = output.clone();
        let inner = RecordingTool::new(spec, move |_| Ok(cloneable.clone()));
        Self { inner }
    }

    /// Returns every recorded invocation, in call order.
    pub fn invocations(&self) -> Vec<ToolRequest> {
        self.inner.invocations()
    }

    /// Returns the number of recorded invocations.
    pub fn call_count(&self) -> usize {
        self.inner.call_count()
    }
}

#[async_trait]
impl Tool for StaticTool {
    fn spec(&self) -> &ToolSpec {
        self.inner.spec()
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        self.inner.invoke(request, ctx).await
    }
}

/// A tool whose [`Tool::invoke`] parks until [`BlockingTool::release`] is
/// called. Sets `entered=true` once the invocation actually started, so
/// tests can synchronise on "the runtime has reached the tool body".
///
/// Cheap to clone — clones share the `entered` flag, the `Notify` and the
/// invocation log.
#[derive(Clone)]
pub struct BlockingTool {
    spec: ToolSpec,
    entered: Arc<AtomicBool>,
    release: Arc<Notify>,
    output: Arc<dyn Fn() -> ToolOutput + Send + Sync + 'static>,
    invocations: Arc<Mutex<Vec<ToolRequest>>>,
}

impl BlockingTool {
    /// Build a blocking tool with `name` returning a constant text output.
    pub fn text(name: impl Into<String>, output: impl Into<String>) -> Self {
        let text = output.into();
        Self::new(name, move || ToolOutput::text(text.clone()))
    }

    /// Build a blocking tool with a custom output factory.
    pub fn new(
        name: impl Into<String>,
        output: impl Fn() -> ToolOutput + Send + Sync + 'static,
    ) -> Self {
        let spec = ToolSpec::new(
            ToolName::new(name),
            "blocking tool used by deferred-result tests",
            json!({ "type": "object", "additionalProperties": true }),
        );
        Self {
            spec,
            entered: Arc::new(AtomicBool::new(false)),
            release: Arc::new(Notify::new()),
            output: Arc::new(output),
            invocations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Returns true once the tool's `invoke` started running.
    pub fn entered(&self) -> bool {
        self.entered.load(Ordering::SeqCst)
    }

    /// Wake the parked invocation so it returns its scripted output.
    /// Uses [`Notify::notify_one`] so a release issued before the
    /// invocation has parked is preserved as a permit and consumed when
    /// the invocation eventually awaits.
    pub fn release(&self) {
        self.release.notify_one();
    }

    /// Spin-poll up to ~1s for `entered` to flip. Test-only convenience.
    pub async fn wait_until_entered(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !self.entered() {
            if std::time::Instant::now() > deadline {
                panic!("BlockingTool '{}' never entered", self.spec.name.0);
            }
            tokio::task::yield_now().await;
        }
    }

    pub fn invocations(&self) -> Vec<ToolRequest> {
        self.invocations.lock().unwrap().clone()
    }
}

#[async_trait]
impl Tool for BlockingTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        self.invocations.lock().unwrap().push(request.clone());
        self.entered.store(true, Ordering::SeqCst);
        self.release.notified().await;
        let output = (self.output)();
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            output,
        )))
    }
}

/// Tool-name keyed [`TaskRoutingPolicy`]. Tools without an explicit entry
/// default to [`RoutingDecision::Foreground`].
#[derive(Clone, Default)]
pub struct NameRoutingPolicy {
    routes: HashMap<String, RoutingDecision>,
}

impl NameRoutingPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn route(mut self, name: impl Into<String>, decision: RoutingDecision) -> Self {
        self.routes.insert(name.into(), decision);
        self
    }
}

impl TaskRoutingPolicy for NameRoutingPolicy {
    fn route(&self, request: &ToolRequest) -> RoutingDecision {
        self.routes
            .get(&request.tool_name.0)
            .copied()
            .unwrap_or(RoutingDecision::Foreground)
    }
}
