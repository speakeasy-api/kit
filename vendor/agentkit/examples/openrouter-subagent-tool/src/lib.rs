use std::error::Error;
use std::sync::{Arc, Mutex};

use agentkit_core::{Item, ItemKind, Part, ToolOutput, ToolResultPart};
use agentkit_loop::{
    Agent, AgentEvent, LoopInterrupt, LoopObserver, LoopStep, ObservedEvent, PromptCacheRequest,
    PromptCacheRetention, SessionConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use agentkit_tools_core::{
    PermissionChecker, PermissionDecision, Tool, ToolAnnotations, ToolContext, ToolError,
    ToolRegistry, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

const DEFAULT_PROMPT: &str =
    "Retrieve the sealed launch code via the Subagent tool and return only the code.";
const ROOT_SYSTEM_PROMPT: &str = "\
You are the root agent.
You do not know the sealed launch code.
The only way to obtain it is by calling the Subagent tool.
Do not guess.
Once the tool returns, respond with only the exact launch code and no other text.
";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeRun {
    pub output: String,
    pub tool_calls: Vec<String>,
}

pub async fn run_probe(secret: &str, prompt: Option<&str>) -> Result<ProbeRun, Box<dyn Error>> {
    dotenvy::dotenv().ok();

    let config = OpenRouterConfig::from_env()?
        .with_temperature(0.0)
        .with_max_completion_tokens(256);
    let adapter = OpenRouterAdapter::new(config)?;
    let observer = RecordingObserver::default();
    let tools = ToolRegistry::new().with(SubagentTool::new(adapter.clone(), secret));

    let agent = Agent::builder()
        .model(adapter)
        .add_tool_source(tools)
        .permissions(AllowAll)
        .observer(observer.clone())
        .transcript(vec![Item::text(ItemKind::System, ROOT_SYSTEM_PROMPT)])
        .input(vec![Item::text(
            ItemKind::User,
            prompt.unwrap_or(DEFAULT_PROMPT),
        )])
        .build()?;

    let mut driver = agent
        .start(SessionConfig::new("openrouter-subagent-tool").with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ))
        .await?;

    let output = run_to_completion(&mut driver).await?;

    Ok(ProbeRun {
        output,
        tool_calls: observer.tool_calls(),
    })
}

pub fn default_prompt() -> &'static str {
    DEFAULT_PROMPT
}

#[derive(Clone)]
struct RecordingObserver {
    tool_calls: Arc<Mutex<Vec<String>>>,
}

impl Default for RecordingObserver {
    fn default() -> Self {
        Self {
            tool_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl RecordingObserver {
    fn tool_calls(&self) -> Vec<String> {
        self.tool_calls.lock().unwrap().clone()
    }
}

impl LoopObserver for RecordingObserver {
    fn handle_event(&self, event: ObservedEvent) {
        let event = event.event;
        if let AgentEvent::ToolCallRequested(call) = event {
            self.tool_calls.lock().unwrap().push(call.name);
        }
    }
}

struct AllowAll;

impl PermissionChecker for AllowAll {
    fn evaluate(
        &self,
        _request: &dyn agentkit_tools_core::PermissionRequest,
    ) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

#[derive(Clone)]
struct SubagentTool {
    adapter: OpenRouterAdapter,
    spec: ToolSpec,
    system_prompt_template: String,
}

impl SubagentTool {
    fn new(adapter: OpenRouterAdapter, secret: &str) -> Self {
        Self {
            adapter,
            spec: ToolSpec::new(
                "Subagent",
                "Run a focused sub-agent that has access to private context unavailable to the root agent.",
                json!({
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string" }
                    },
                    "required": ["prompt"],
                    "additionalProperties": false
                }),
            )
            .with_annotations(ToolAnnotations::new()),
            system_prompt_template: format!(
                "You are a sealed sub-agent.\nThe private launch code is {secret}.\nThe root agent does not know this code.\nIf the user asks for the launch code, answer with exactly {secret} and no other text."
            ),
        }
    }
}

#[derive(Deserialize)]
struct SubagentInput {
    prompt: String,
}

#[async_trait]
impl Tool for SubagentTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: SubagentInput = serde_json::from_value(request.input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;

        let agent = Agent::builder()
            .model(self.adapter.clone())
            .permissions(AllowAll)
            .transcript(vec![Item::text(
                ItemKind::System,
                &self.system_prompt_template,
            )])
            .input(vec![Item::text(ItemKind::User, &input.prompt)])
            .build()
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;

        let mut driver = agent
            .start(
                SessionConfig::new(format!(
                    "{}-subagent-{}",
                    request.session_id, request.call_id
                ))
                .with_cache(
                    PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
                ),
            )
            .await
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;

        let output = run_to_completion(&mut driver)
            .await
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;

        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::text(output),
        )))
    }
}

async fn run_to_completion<S>(
    driver: &mut agentkit_loop::LoopDriver<S>,
) -> Result<String, Box<dyn Error>>
where
    S: agentkit_loop::ModelSession,
{
    loop {
        match driver.next().await? {
            LoopStep::Finished(result) => return Ok(collect_assistant_output(&result.items)),
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                return Err(
                    format!("unexpected approval request: {}", pending.request.summary).into(),
                );
            }
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                return Err("loop requested more input before finishing".into());
            }
        }
    }
}

fn collect_assistant_output(items: &[Item]) -> String {
    let mut sections = Vec::new();

    for item in items {
        if item.kind != ItemKind::Assistant {
            continue;
        }

        for part in &item.parts {
            match part {
                Part::Text(text) => sections.push(text.text.clone()),
                Part::Structured(value) => sections.push(value.value.to_string()),
                _ => {}
            }
        }
    }

    sections.join("\n")
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;

    #[tokio::test]
    #[ignore = "requires OPENROUTER_API_KEY and a live OpenRouter model"]
    async fn root_agent_retrieves_secret_via_subagent() {
        let secret = env::var("SUBAGENT_SECRET").unwrap_or_else(|_| "LANTERN-SECRET-93B7".into());

        let run = run_probe(&secret, None).await.unwrap();

        assert!(
            run.tool_calls.iter().any(|name| name == "Subagent"),
            "expected the root agent to call Subagent, saw {:?}",
            run.tool_calls
        );
        assert!(
            run.output.contains(&secret),
            "expected root output to contain {secret}, got {:?}",
            run.output
        );
    }
}
