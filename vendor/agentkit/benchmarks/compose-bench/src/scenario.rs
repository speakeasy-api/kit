//! Scenario abstractions shared by every benchmark case.
//!
//! A scenario owns a deterministic in-memory "world" (the mock SaaS backend),
//! exposes it through granular tools, and scores the run afterwards from the
//! world's final state plus whatever the agent submitted via `submit_result`.

use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    CompositePermissionChecker, Tool, ToolContext, ToolError, ToolName, ToolRegistry, ToolRequest,
    ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde_json::{Value, json};

pub type BenchError = Box<dyn Error + Send + Sync>;

/// Which tool surface the agent gets for a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arm {
    /// Scenario tools only, called one model round-trip at a time.
    Granular,
    /// Scenario tools plus the `compose` Lua tool wrapping them.
    Compose,
    /// Scenario tools plus the `compose` tool running the Runlet backend.
    RunletCompose,
    /// `shell_exec` only (file-backed scenarios); the Bash-pipeline reference.
    Bash,
}

impl Arm {
    pub fn as_str(self) -> &'static str {
        match self {
            Arm::Granular => "granular",
            Arm::Compose => "compose",
            Arm::RunletCompose => "runlet",
            Arm::Bash => "bash",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "granular" => Some(Arm::Granular),
            "compose" => Some(Arm::Compose),
            "runlet" => Some(Arm::RunletCompose),
            "bash" => Some(Arm::Bash),
            _ => None,
        }
    }
}

/// Outcome of a scenario's verifier.
pub struct Score {
    /// 0.0..=1.0; partial credit per scenario-specific rubric.
    pub accuracy: f64,
    /// Human-readable rubric breakdown for the report.
    pub notes: Vec<String>,
}

/// Everything the harness needs to run one fresh attempt at a scenario.
pub struct ScenarioInstance {
    pub tools: ToolRegistry,
    pub user_prompt: String,
    pub permissions: Option<CompositePermissionChecker>,
    /// Slot `submit_result` writes into; the harness checks it for presence.
    pub submission: Submission,
    pub scorer: Box<dyn FnOnce() -> Score + Send>,
}

pub trait Scenario: Send + Sync {
    fn name(&self) -> &'static str;

    /// Arms this scenario supports. `Bash` only makes sense where the world is
    /// reachable from a shell (i.e. file-backed scenarios).
    fn arms(&self) -> Vec<Arm> {
        vec![Arm::Granular, Arm::Compose, Arm::RunletCompose]
    }

    /// Builds a fresh world + tool registry. Must be deterministic.
    fn setup(&self, arm: Arm) -> Result<ScenarioInstance, BenchError>;
}

/// One system prompt for every scenario and arm, deliberately neutral about
/// composition so tool *descriptions* (not the prompt) drive tool preference.
pub const SYSTEM_PROMPT: &str = "\
You are an operations assistant completing a task with the available tools. \
Work autonomously and never ask the user questions. Be efficient: keep the \
number of steps as low as you can. When the task is complete, call the \
`submit_result` tool exactly once with the JSON shape requested in the task, \
then stop.";

static TOOL_LATENCY: OnceLock<Duration> = OnceLock::new();

/// Simulated per-call latency for mock tools, mimicking a remote MCP server.
/// Applied uniformly to every scenario tool in every arm.
pub fn set_tool_latency(latency: Duration) {
    let _ = TOOL_LATENCY.set(latency);
}

fn tool_latency() -> Duration {
    TOOL_LATENCY.get().copied().unwrap_or(Duration::ZERO)
}

/// Error message every flaky tool fails with, shaped like a real transient
/// upstream failure so arms can recognise retryability from the text alone.
pub const TRANSIENT_FAILURE_MESSAGE: &str =
    "503 service unavailable: transient upstream error, retry";

/// A scenario tool backed by a synchronous closure over the shared world.
pub struct FnTool {
    spec: ToolSpec,
    #[allow(clippy::type_complexity)]
    handler: Box<dyn Fn(&Value) -> Result<Value, String> + Send + Sync>,
    /// How many upcoming invocations still fail transiently. Scenarios build a
    /// fresh `FnTool` per run (`Scenario::setup` is called once per rep), so a
    /// plain per-instance counter resets naturally between reps.
    flaky_remaining: AtomicUsize,
    /// Extra per-call latency on top of the global [`set_tool_latency`] value.
    extra_latency: Duration,
}

impl FnTool {
    pub fn new(
        name: &str,
        description: &str,
        input_schema: Value,
        output_schema: Value,
        handler: impl Fn(&Value) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            spec: ToolSpec::new(ToolName::new(name), description, input_schema)
                .with_output_schema(output_schema),
            handler: Box::new(handler),
            flaky_remaining: AtomicUsize::new(0),
            extra_latency: Duration::ZERO,
        }
    }

    /// Marks the tool as a pure read, matching how production tools annotate
    /// themselves. Composition backends rely on this to keep reads lazy while
    /// treating unannotated tools as effectful.
    pub fn read_only(mut self) -> Self {
        self.spec.annotations = self.spec.annotations.clone().with_read_only(true);
        self
    }

    /// The first `n` invocations of this tool fail with a transient
    /// 503-shaped error ([`TRANSIENT_FAILURE_MESSAGE`]), like a real remote
    /// API hiccup. Deterministic: fails exactly `n` times per scenario run,
    /// then succeeds forever, so scenario data and correct answers are
    /// unaffected.
    ///
    /// The failure is raised as [`ToolError::Unavailable`] from `invoke`
    /// rather than through the handler's `Err(String)` path on purpose: a
    /// handler error becomes a *completed* result with `is_error = true`,
    /// which the compose child dispatcher flattens into a plain value
    /// (`is_error` is dropped), so compose scripts would silently receive the
    /// error text as data. A `ToolError` instead surfaces as a failed child
    /// call in both compose backends and as an `is_error` tool result to the
    /// model in the granular arm. `Unavailable` is also the one variant the
    /// runlet backend maps to `retryable = true`, matching 503 semantics.
    pub fn flaky(mut self, n: usize) -> Self {
        self.flaky_remaining = AtomicUsize::new(n);
        self
    }
}

#[async_trait]
impl Tool for FnTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let latency = tool_latency() + self.extra_latency;
        if !latency.is_zero() {
            tokio::time::sleep(latency).await;
        }
        if self
            .flaky_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(ToolError::Unavailable(TRANSIENT_FAILURE_MESSAGE.into()));
        }
        match (self.handler)(&request.input) {
            Ok(output) => Ok(ToolResult::new(ToolResultPart::success(
                request.call_id,
                ToolOutput::structured(output),
            ))),
            Err(message) => Ok(ToolResult::new(ToolResultPart::error(
                request.call_id,
                ToolOutput::text(message),
            ))),
        }
    }
}

/// Shared slot the `submit_result` tool writes the agent's final answer into.
pub type Submission = Arc<Mutex<Option<Value>>>;

/// Builds the `submit_result` tool every scenario registers. `answer_schema`
/// describes the scenario-specific payload so the model knows the exact shape.
pub fn submit_result_tool(answer_schema: Value) -> (FnTool, Submission) {
    let submission: Submission = Arc::new(Mutex::new(None));
    let slot = submission.clone();
    let tool = FnTool::new(
        "submit_result",
        "Submit the final answer for the task. Call exactly once, when the task is complete.",
        json!({
            "type": "object",
            "properties": { "answer": answer_schema },
            "required": ["answer"],
            "additionalProperties": false
        }),
        json!({
            "type": "object",
            "properties": { "recorded": { "type": "boolean" } },
            "required": ["recorded"]
        }),
        move |input| {
            let answer = input
                .get("answer")
                .cloned()
                .ok_or_else(|| "missing required field `answer`".to_string())?;
            *slot.lock().expect("submission lock") = Some(answer);
            Ok(json!({ "recorded": true }))
        },
    );
    (tool, submission)
}

/// 1-based pagination envelope used by the mock list endpoints.
pub fn paginate(items: Vec<Value>, page: u64, per_page: usize) -> Value {
    let total_items = items.len();
    let total_pages = total_items.div_ceil(per_page).max(1);
    let page = page.max(1) as usize;
    let start = (page - 1).saturating_mul(per_page);
    let slice: Vec<Value> = items.into_iter().skip(start).take(per_page).collect();
    json!({
        "items": slice,
        "page": page,
        "per_page": per_page,
        "total_pages": total_pages,
        "total_items": total_items,
    })
}

/// Output schema for the [`paginate`] envelope.
pub fn page_schema(item_schema: Value) -> Value {
    json!({
        "type": "object",
        "properties": {
            "items": { "type": "array", "items": item_schema },
            "page": { "type": "integer" },
            "per_page": { "type": "integer" },
            "total_pages": { "type": "integer" },
            "total_items": { "type": "integer" }
        },
        "required": ["items", "page", "total_pages", "total_items"]
    })
}

pub fn get_u64(input: &Value, key: &str, default: u64) -> u64 {
    input.get(key).and_then(Value::as_u64).unwrap_or(default)
}

pub fn get_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required string field `{key}`"))
}

/// Symmetric set-overlap score (F1) between a submitted and expected id set.
pub fn f1(submitted: &[String], expected: &[String]) -> f64 {
    if expected.is_empty() {
        return if submitted.is_empty() { 1.0 } else { 0.0 };
    }
    if submitted.is_empty() {
        return 0.0;
    }
    let hits = submitted.iter().filter(|id| expected.contains(id)).count() as f64;
    let precision = hits / submitted.len() as f64;
    let recall = hits / expected.len() as f64;
    if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    }
}

#[cfg(test)]
mod tests {
    use agentkit_core::{MetadataMap, SessionId, ToolCallId, TurnId};
    use agentkit_tools_core::{AllowAllPermissions, OwnedToolContext};

    use super::*;

    fn probe_tool() -> FnTool {
        FnTool::new(
            "probe",
            "test tool",
            json!({ "type": "object" }),
            json!({ "type": "object" }),
            |_input| Ok(json!({ "ok": true })),
        )
        .read_only()
    }

    fn request(call: &str) -> ToolRequest {
        ToolRequest {
            call_id: ToolCallId::new(call),
            tool_name: ToolName::new("probe"),
            input: json!({}),
            session_id: SessionId::new("s"),
            turn_id: TurnId::new("t"),
            metadata: MetadataMap::new(),
        }
    }

    fn test_context() -> OwnedToolContext {
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
    async fn flaky_fails_first_call_then_succeeds() {
        let tool = probe_tool().flaky(1);
        let owned = test_context();

        let error = tool
            .invoke(request("c1"), &mut owned.borrowed())
            .await
            .expect_err("first call must fail transiently");
        assert!(matches!(error, ToolError::Unavailable(_)));
        assert!(error.to_string().contains(TRANSIENT_FAILURE_MESSAGE));

        let result = tool
            .invoke(request("c2"), &mut owned.borrowed())
            .await
            .expect("second call must succeed");
        assert!(!result.result.is_error);

        let result = tool
            .invoke(request("c3"), &mut owned.borrowed())
            .await
            .expect("later calls keep succeeding");
        assert!(!result.result.is_error);
    }

    #[tokio::test]
    async fn non_flaky_tool_never_fails() {
        let tool = probe_tool();
        let owned = test_context();
        let result = tool
            .invoke(request("c1"), &mut owned.borrowed())
            .await
            .expect("call must succeed");
        assert!(!result.result.is_error);
    }
}
