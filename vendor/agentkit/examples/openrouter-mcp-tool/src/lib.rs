use std::env;
use std::error::Error;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agentkit_core::{Item, ItemKind, Part};
use agentkit_loop::{
    Agent, AgentEvent, LoopInterrupt, LoopObserver, LoopStep, ObservedEvent, PromptCacheRequest,
    PromptCacheRetention, SessionConfig,
};
use agentkit_mcp::{
    McpServerConfig, McpServerId, McpServerManager, McpTransportBinding, StdioTransportConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use serde_json::{Value, json};
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

const DEFAULT_PROMPT: &str =
    "Retrieve the sealed launch code via the MCP tool and return only the code.";
const ROOT_SYSTEM_PROMPT: &str = "\
You are the root agent.
You do not know the sealed launch code.
The only way to obtain it is by calling the MCP tool mcp_mock_reveal_secret.
Do not guess.
Once the tool returns, respond with only the exact launch code and no other text.
";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeRun {
    pub output: String,
    pub tool_calls: Vec<String>,
}

pub async fn run_probe(secret: &str, prompt: Option<&str>) -> Result<ProbeRun, Box<dyn Error>> {
    run_probe_with_command(secret, prompt, env::current_exe()?).await
}

pub async fn run_probe_with_command(
    secret: &str,
    prompt: Option<&str>,
    command: impl AsRef<Path>,
) -> Result<ProbeRun, Box<dyn Error>> {
    dotenvy::dotenv().ok();

    let config = OpenRouterConfig::from_env()?
        .with_temperature(0.0)
        .with_max_completion_tokens(256);
    let adapter = OpenRouterAdapter::new(config)?;
    let observer = RecordingObserver::default();
    let mut manager = McpServerManager::new().with_server(McpServerConfig::new(
        "mock",
        McpTransportBinding::Stdio(
            StdioTransportConfig::new(command.as_ref().display().to_string())
                .with_arg("--serve-mock-mcp")
                .with_env("MCP_SECRET", secret),
        ),
    ));
    manager.connect_server(&McpServerId::new("mock")).await?;

    let agent = Agent::builder()
        .model(adapter)
        .add_tool_source(manager.source())
        .observer(observer.clone())
        .transcript(vec![Item::text(ItemKind::System, ROOT_SYSTEM_PROMPT)])
        .input(vec![Item::text(
            ItemKind::User,
            prompt.unwrap_or(DEFAULT_PROMPT),
        )])
        .build()?;

    let mut driver = agent
        .start(SessionConfig::new("openrouter-mcp-tool").with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ))
        .await?;

    let output = run_to_completion(&mut driver).await?;
    manager.disconnect_server(&McpServerId::new("mock")).await?;

    Ok(ProbeRun {
        output,
        tool_calls: observer.tool_calls(),
    })
}

pub async fn run_mock_server_from_env() -> Result<(), Box<dyn Error>> {
    let secret = env::var("MCP_SECRET").unwrap_or_else(|_| "LANTERN-SECRET-93B7".into());
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut writer = stdout;
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        let message: Value = serde_json::from_str(line.trim())?;
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };

        match method {
            "initialize" => {
                write_response(
                    &mut writer,
                    message["id"].clone(),
                    json!({ "capabilities": {} }),
                )
                .await?;
            }
            "notifications/initialized" => {}
            "tools/list" => {
                write_response(
                    &mut writer,
                    message["id"].clone(),
                    json!({
                        "tools": [{
                            "name": "reveal_secret",
                            "description": "Reveal the sealed launch code.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {},
                                "additionalProperties": false
                            }
                        }]
                    }),
                )
                .await?;
            }
            "resources/list" => {
                write_response(
                    &mut writer,
                    message["id"].clone(),
                    json!({ "resources": [] }),
                )
                .await?;
            }
            "prompts/list" => {
                write_response(&mut writer, message["id"].clone(), json!({ "prompts": [] }))
                    .await?;
            }
            "tools/call" => {
                let name = message["params"]["name"].as_str().unwrap_or_default();
                let result = if name == "reveal_secret" {
                    json!({
                        "content": [{
                            "type": "text",
                            "text": secret,
                        }]
                    })
                } else {
                    json!({
                        "content": [{
                            "type": "text",
                            "text": format!("unknown tool: {name}"),
                        }]
                    })
                };
                write_response(&mut writer, message["id"].clone(), result).await?;
            }
            _ => {
                if let Some(id) = message.get("id").cloned() {
                    write_error(&mut writer, id, -32601, format!("unknown method: {method}"))
                        .await?;
                }
            }
        }
    }

    Ok(())
}

pub fn default_prompt() -> &'static str {
    DEFAULT_PROMPT
}

async fn write_response(
    writer: &mut tokio::io::Stdout,
    id: Value,
    result: Value,
) -> Result<(), Box<dyn Error>> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    writer.write_all(payload.to_string().as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn write_error(
    writer: &mut tokio::io::Stdout,
    id: Value,
    code: i64,
    message: String,
) -> Result<(), Box<dyn Error>> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    });
    writer.write_all(payload.to_string().as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
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
