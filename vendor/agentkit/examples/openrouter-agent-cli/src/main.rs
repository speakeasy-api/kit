use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;

use agentkit_compaction::{
    AgentBuilderCompactorExt, CompactionPipeline, DropFailedToolResultsStrategy,
    DropReasoningStrategy, KeepRecentStrategy, StrategyCompactor,
};
use agentkit_context::{AgentsMd, ContextLoader};
use agentkit_core::{Item, ItemKind, MetadataMap, Part};
use agentkit_loop::{
    Agent, LoopInterrupt, LoopStep, PromptCacheRequest, PromptCacheRetention, SessionConfig,
};
use agentkit_mcp::{
    McpServerConfig, McpServerId, McpServerManager, McpTransportBinding, StdioTransportConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use agentkit_reporting::{CompositeReporter, StdoutReporter};
use agentkit_tool_skills::SkillRegistry;
use agentkit_tools_core::{
    CommandPolicy, CompositePermissionChecker, PathPolicy, PermissionCode, PermissionDecision,
    PermissionDenial,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const SYSTEM_PROMPT: &str = "\
You are a careful coding agent.
Use loaded context as authoritative project guidance.
Use fs.* tools for repository inspection and edits.
Use shell_exec only for simple read-only inspection commands when that is more appropriate than fs tools.
If an MCP tool is available and relevant, use it instead of guessing.
Prefer concise answers and avoid making claims you did not verify.
";

const DEFAULT_PROMPT: &str = "Use fs_read_file on ./Cargo.toml and report the workspace member count. Return only the integer.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();

    let args = Args::parse()?;
    if args.serve_mock_mcp {
        return run_mock_server().await;
    }

    let workspace_root = env::current_dir()?;
    let context_root = args.context_root.unwrap_or_else(|| workspace_root.clone());
    let context_items = ContextLoader::new()
        .with_source(
            AgentsMd::discover_all(&context_root).with_search_dir(context_root.join(".agent")),
        )
        .load()
        .await?;

    // Discover skills progressively — catalog in tool description, body on demand.
    let skill_registry = SkillRegistry::from_paths(vec![
        context_root.join("skills"),
        context_root.join(".agent/skills"),
        context_root.join(".agents/skills"),
    ])
    .discover_skills()
    .await;

    let config = OpenRouterConfig::from_env()?
        .with_temperature(0.0)
        .with_max_completion_tokens(512);
    let adapter = OpenRouterAdapter::new(config)?;
    let reporter =
        CompositeReporter::new().with_observer(StdoutReporter::new(io::stderr()).with_usage(false));

    let tools = agentkit_tool_fs::registry()
        .merge(agentkit_tool_shell::registry())
        .merge(skill_registry.tool_registry());

    let mut manager = if args.mcp_mock {
        let mut manager = McpServerManager::new().with_server(McpServerConfig::new(
            "mock",
            McpTransportBinding::Stdio(
                StdioTransportConfig::new(env::current_exe()?.display().to_string())
                    .with_arg("--serve-mock-mcp")
                    .with_env("MCP_SECRET", "LANTERN-SECRET-93B7"),
            ),
        ));
        manager.connect_server(&McpServerId::new("mock")).await?;
        Some(manager)
    } else {
        None
    };

    let permissions = CompositePermissionChecker::new(PermissionDecision::Deny(PermissionDenial {
        code: PermissionCode::UnknownRequest,
        message: "tool action is not allowed by the mini CLI policy".into(),
        metadata: MetadataMap::new(),
    }))
    .with_policy(
        PathPolicy::new()
            .allow_root(workspace_root.clone())
            .require_approval_outside_allowed(false),
    )
    .with_policy(
        CommandPolicy::new()
            .allow_cwd(workspace_root.clone())
            .allow_executable("pwd")
            .allow_executable("ls")
            .allow_executable("cat")
            .require_approval_for_unknown(false),
    );

    let mut builder = Agent::builder()
        .model(adapter)
        .add_tool_source(tools)
        .permissions(permissions)
        .compactor(
            StrategyCompactor::builder()
                .item_count_trigger(12)
                .strategy(
                    CompactionPipeline::new()
                        .with_strategy(DropReasoningStrategy::new())
                        .with_strategy(DropFailedToolResultsStrategy::new())
                        .with_strategy(
                            KeepRecentStrategy::new(8)
                                .preserve_kind(ItemKind::System)
                                .preserve_kind(ItemKind::Context),
                        ),
                )
                .build()?,
        )
        .observer(reporter);

    if let Some(manager) = manager.as_ref() {
        builder = builder.add_tool_source(manager.source());
    }

    let mut transcript = vec![Item::text(ItemKind::System, SYSTEM_PROMPT)];
    transcript.extend(context_items);

    let agent = builder
        .transcript(transcript)
        .input(vec![Item::text(ItemKind::User, &args.prompt)])
        .build()?;

    let mut driver = agent
        .start(SessionConfig::new("openrouter-agent-cli").with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ))
        .await?;

    let result = run_to_completion(&mut driver).await;

    if let Some(manager) = manager.as_mut() {
        let _ = manager.disconnect_server(&McpServerId::new("mock")).await;
    }

    result
}

struct Args {
    context_root: Option<PathBuf>,
    mcp_mock: bool,
    serve_mock_mcp: bool,
    prompt: String,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut args = env::args().skip(1);
        let mut context_root = None;
        let mut mcp_mock = false;
        let mut serve_mock_mcp = false;
        let mut prompt_parts = Vec::new();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--context-root" => {
                    let value = args.next().ok_or("missing value for --context-root")?;
                    context_root = Some(PathBuf::from(value));
                }
                "--mcp-mock" => mcp_mock = true,
                "--serve-mock-mcp" => serve_mock_mcp = true,
                _ => {
                    prompt_parts.push(arg);
                    prompt_parts.extend(args);
                    break;
                }
            }
        }

        Ok(Self {
            context_root,
            mcp_mock,
            serve_mock_mcp,
            prompt: if prompt_parts.is_empty() {
                DEFAULT_PROMPT.into()
            } else {
                prompt_parts.join(" ")
            },
        })
    }
}

async fn run_to_completion<S>(
    driver: &mut agentkit_loop::LoopDriver<S>,
) -> Result<(), Box<dyn Error>>
where
    S: agentkit_loop::ModelSession,
{
    loop {
        match driver.next().await? {
            LoopStep::Finished(result) => {
                for item in result.items {
                    if item.kind == ItemKind::Assistant {
                        print_assistant_item(item);
                    }
                }
                return Ok(());
            }
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

fn print_assistant_item(item: Item) {
    let mut saw_output = false;

    for part in item.parts {
        match part {
            Part::Text(text) => {
                if !saw_output {
                    println!("[output]");
                    saw_output = true;
                }
                println!("{}", text.text);
            }
            Part::Reasoning(reasoning) => {
                if let Some(summary) = reasoning.summary {
                    println!("[reasoning]");
                    println!("{summary}");
                }
            }
            Part::Structured(value) => {
                if !saw_output {
                    println!("[output]");
                    saw_output = true;
                }
                println!("{}", value.value);
            }
            Part::ToolCall(call) => {
                println!("[tool call] {} {}", call.name, call.input);
            }
            Part::Media(_) | Part::File(_) | Part::ToolResult(_) | Part::Custom(_) => {}
        }
    }
}

async fn run_mock_server() -> Result<(), Box<dyn Error>> {
    let secret = env::var("MCP_SECRET").unwrap_or_else(|_| "LANTERN-SECRET-93B7".into());
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
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
