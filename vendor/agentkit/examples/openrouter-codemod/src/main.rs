//! Bulk codemod driven by the `compose` Lua tool.
//!
//! This example demonstrates [`agentkit_tool_compose`] by asking an OpenRouter
//! model to perform a many-file codemod in a single tool call: list a scratch
//! directory, read each file, transform the text, and write it back, all from
//! one Lua script the model authors at runtime.
//!
//! Run with:
//!
//! ```bash
//! cargo run -p openrouter-codemod
//! ```
//!
//! Required env vars (loaded from the workspace `.env` via dotenvy):
//! `OPENROUTER_API_KEY` and `OPENROUTER_MODEL`.

use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};

use agentkit_core::{Item, ItemKind, MetadataMap, Part, ToolOutput};
use agentkit_loop::{
    Agent, LoopInterrupt, LoopStep, PromptCacheRequest, PromptCacheRetention, SessionConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use agentkit_tools_core::{
    CommandPolicy, CompositePermissionChecker, PathPolicy, PermissionCode, PermissionDecision,
    PermissionDenial,
};

const SYSTEM_PROMPT: &str = "\
You are a codemod assistant. You have access to a `compose` tool that runs a
sandboxed Lua script over the available tool catalog. The script calls
`tool(name, input)` synchronously and can inspect the catalog with `tools()`.

For codemods, prefer ONE compose call that lists the target directory, reads
each file, applies the transformation, and writes the result back. Return a
JSON object from the Lua script that summarises what changed. Available file
tools include fs_list_directory, fs_read_file, fs_write_file, fs_replace_in_file.
";

/// Hard-coded templates we drop into the scratch directory before each run so
/// the demo is deterministic and self-contained.
const SAMPLES: &[(&str, &str)] = &[
    (
        "alpha.rs",
        "fn main() {\n    println!(\"alpha starting\");\n    println!(\"alpha done\");\n}\n",
    ),
    (
        "beta.rs",
        "pub fn beta(name: &str) {\n    println!(\"hello, {name}\");\n}\n",
    ),
    (
        "gamma.rs",
        "fn check(value: i32) {\n    if value > 0 {\n        println!(\"positive: {value}\");\n    } else {\n        println!(\"non-positive: {value}\");\n    }\n}\n",
    ),
    (
        "delta.rs",
        "fn log_pair(key: &str, value: &str) {\n    println!(\"{key}={value}\");\n}\n",
    ),
    (
        "epsilon.rs",
        "fn step() {\n    println!(\"step 1\");\n    println!(\"step 2\");\n    println!(\"step 3\");\n}\n",
    ),
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();

    let workspace_root = env::current_dir()?;
    let scratch = prepare_scratch(&workspace_root).await?;
    println!("scratch directory: {}", scratch.display());

    let config = OpenRouterConfig::from_env()?;
    let model_name = config.model.clone();
    let adapter = OpenRouterAdapter::new(config)?;

    // ComposeTool::wrap renders each child tool's output schema into the compose
    // tool description, so the model sees the exact return shape of every tool it
    // might call from Lua. Wrapped tools are exposed individually.
    let tool_source = agentkit_tool_compose::ComposeTool::wrap(
        agentkit_tool_fs::registry().merge(agentkit_tool_shell::registry()),
    );

    // Restrict the demo to the scratch directory. Anything outside falls
    // through to the approval-required path (which would surface as a
    // LoopInterrupt::ApprovalRequest below); inside the whitelist no approval
    // is required so the demo runs to completion non-interactively.
    let permissions = CompositePermissionChecker::new(PermissionDecision::Deny(PermissionDenial {
        code: PermissionCode::UnknownRequest,
        message: "tool request is not covered by any policy".into(),
        metadata: MetadataMap::new(),
    }))
    .with_policy(
        PathPolicy::new()
            .allow_root(scratch.clone())
            .require_approval_outside_allowed(true),
    )
    .with_policy(
        CommandPolicy::new()
            .allow_cwd(scratch.clone())
            .require_approval_for_unknown(true),
    );

    let user_prompt = format!(
        "Use the `compose` tool to write ONE Lua script that scans `{scratch}`, rewrites every \
         `println!(...)` call in every `.rs` file to `tracing::info!(...)` (preserving the \
         argument list verbatim), and returns a structured summary \
         `{{ files_changed = <integer>, replacements = <integer> }}`. Use `tools()` if you need \
         to discover the exact tool names and inputs. Use fs_list_directory then fs_read_file + \
         fs_write_file (or fs_replace_in_file) for each file. Do not call any tools outside the \
         compose script.",
        scratch = scratch.display()
    );

    let agent = Agent::builder()
        .model(adapter)
        .add_tool_source(tool_source)
        .permissions(permissions)
        .transcript(vec![Item::text(ItemKind::System, SYSTEM_PROMPT)])
        .input(vec![Item::text(ItemKind::User, user_prompt)])
        .build()?;

    print_banner(&model_name, &scratch);

    let mut driver = agent
        .start(SessionConfig::new("openrouter-codemod").with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ))
        .await?;

    let mut cursor = 0usize;
    loop {
        match driver.next().await? {
            LoopStep::Finished(_) => {
                render_transcript(&driver.snapshot().transcript, &mut cursor);
                break;
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {
                render_transcript(&driver.snapshot().transcript, &mut cursor);
            }
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                eprintln!("[unexpected: model asked for more input; ending demo]");
                break;
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                eprintln!(
                    "[approval requested for {}: {} — the demo refuses to step outside the \
                     scratch directory; aborting]",
                    pending.request.request_kind, pending.request.summary
                );
                break;
            }
        }
    }

    Ok(())
}

/// Streams transcript items the loop produced since the last call. Renders
/// tool calls (esp. the compose Lua script) and tool results (esp. the
/// structured summary) inline — those are the artifacts a compose demo
/// actually exists to surface.
fn render_transcript(transcript: &[Item], cursor: &mut usize) {
    for item in &transcript[*cursor..] {
        for part in &item.parts {
            match part {
                Part::Text(text)
                    if matches!(item.kind, ItemKind::Assistant) && !text.text.trim().is_empty() =>
                {
                    println!("\nassistant> {}", text.text.trim_end());
                }
                Part::ToolCall(call) => {
                    println!("\n[tool call] {} (id={})", call.name, call.id.0.as_str());
                    if call.name == agentkit_tool_compose::COMPOSE_TOOL_NAME {
                        if let Some(script) = call.input.get("script").and_then(|v| v.as_str()) {
                            println!("--- lua script ---");
                            println!("{}", script.trim_end());
                            println!("--- end script ---");
                        }
                    } else {
                        match serde_json::to_string(&call.input) {
                            Ok(s) => println!("  input: {s}"),
                            Err(_) => println!("  input: <unserialisable>"),
                        }
                    }
                }
                Part::ToolResult(result) => {
                    let label = if result.is_error {
                        "tool error"
                    } else {
                        "tool result"
                    };
                    println!("\n[{label}] call_id={}", result.call_id.0.as_str());
                    match &result.output {
                        ToolOutput::Text(text) => println!("{}", text.trim_end()),
                        ToolOutput::Structured(value) => {
                            match serde_json::to_string_pretty(value) {
                                Ok(s) => println!("{s}"),
                                Err(_) => println!("{value}"),
                            }
                        }
                        other => println!("<{other:?}>"),
                    }
                }
                _ => (),
            }
        }
    }
    *cursor = transcript.len();
}

/// Recreate the scratch directory and populate it with [`SAMPLES`]. We delete
/// any prior state so reruns are deterministic.
async fn prepare_scratch(workspace_root: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let scratch = workspace_root.join("target").join("codemod-demo");
    if tokio::fs::metadata(&scratch).await.is_ok() {
        tokio::fs::remove_dir_all(&scratch).await?;
    }
    tokio::fs::create_dir_all(&scratch).await?;
    for (name, body) in SAMPLES {
        tokio::fs::write(scratch.join(name), body).await?;
    }
    Ok(scratch.canonicalize()?)
}

fn print_banner(model: &str, scratch: &Path) {
    println!("openrouter-codemod  ({model})");
    println!("scope: {}", scratch.display());
    println!("strategy: one compose call rewrites println! -> tracing::info! across all .rs files");
    println!();
}
