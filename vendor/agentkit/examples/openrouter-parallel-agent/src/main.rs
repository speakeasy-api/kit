use std::env;
use std::io::{self, Write as _};
use std::time::Duration;

use agentkit_core::{Item, ItemKind, MetadataMap, Part};
use agentkit_loop::{
    Agent, LoopInterrupt, LoopStep, PromptCacheRequest, PromptCacheRetention, SessionConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use agentkit_reporting::{CompositeReporter, StdoutReporter};
use agentkit_task_manager::{AsyncTaskManager, RoutingDecision, TaskEvent, TaskManager};
use agentkit_tools_core::{
    CommandPolicy, CompositePermissionChecker, PathPolicy, PermissionCode, PermissionDecision,
    PermissionDenial,
};

const SYSTEM_PROMPT: &str = "\
You are a repository assistant with filesystem and shell tools.
Use fs.* tools for reading, writing, and listing files.
Use shell_exec for commands like `find`, `wc`, `grep`, `sleep`, or anything that benefits from the shell.
When calling shell_exec, set `executable` to the command name or script path directly (e.g. \"sleep\", \"find\", \"./examples/openrouter-parallel-agent/scripts/delayed-secret.sh\") — never use `/bin/sh` or other shell wrappers.
A script at ./examples/openrouter-parallel-agent/scripts/delayed-secret.sh prints a secret value after a delay. You can run it directly as the executable.
When the user asks about multiple files, read them all — don't stop at one.
Prefer concise answers.
";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let prompt = env::args().skip(1).collect::<Vec<_>>().join(" ");
    if prompt.trim().is_empty() {
        return Err("usage: cargo run -p openrouter-parallel-agent -- '<prompt>'".into());
    }

    let config = OpenRouterConfig::from_env()?;
    let adapter = OpenRouterAdapter::new(config)?;

    // --- tools: filesystem + shell ---
    let tools = agentkit_tool_fs::registry().merge(agentkit_tool_shell::registry());

    // --- permissions ---
    let workspace_root = env::current_dir()?;
    let permissions = CompositePermissionChecker::new(PermissionDecision::Deny(PermissionDenial {
        code: PermissionCode::UnknownRequest,
        message: "action is not allowed by the parallel-agent policy".into(),
        metadata: MetadataMap::new(),
    }))
    .with_policy(
        PathPolicy::new()
            .allow_root(workspace_root.clone())
            .require_approval_outside_allowed(false),
    )
    .with_policy(
        CommandPolicy::new()
            .allow_cwd(workspace_root)
            .allow_executable("pwd")
            .allow_executable("ls")
            .allow_executable("cat")
            .allow_executable("find")
            .allow_executable("wc")
            .allow_executable("grep")
            .allow_executable("echo")
            .allow_executable("sleep")
            .allow_executable("./examples/openrouter-parallel-agent/scripts/delayed-secret.sh"),
    );

    // --- async task manager ---
    let task_manager = AsyncTaskManager::new().routing(|req: &agentkit_tools_core::ToolRequest| {
        if req.tool_name.0 == "shell_exec" {
            // Shell commands start foreground; detach after 2s if still running.
            RoutingDecision::ForegroundThenDetachAfter(Duration::from_secs(2))
        } else {
            RoutingDecision::Foreground
        }
    });
    let event_handle = task_manager.handle();
    let idle_handle = task_manager.handle();

    // --- reporter ---
    let reporter =
        CompositeReporter::new().with_observer(StdoutReporter::new(io::stderr()).with_usage(false));

    // --- build agent ---
    let agent = Agent::builder()
        .model(adapter)
        .add_tool_source(tools)
        .task_manager(task_manager)
        .permissions(permissions)
        .observer(reporter)
        .transcript(vec![system_item()])
        .input(vec![user_item(&prompt)])
        .build()?;

    // --- spawn task-event printer ---
    tokio::spawn(async move {
        while let Some(event) = event_handle.next_event().await {
            let line = match &event {
                TaskEvent::Started(snap) => {
                    format!(
                        "[task {}] started  tool={} kind={:?}",
                        snap.id, snap.tool_name, snap.kind
                    )
                }
                TaskEvent::Detached(snap) => {
                    format!(
                        "[task {}] detached tool={} (promoted to background)",
                        snap.id, snap.tool_name
                    )
                }
                TaskEvent::Completed(snap, _result) => {
                    format!("[task {}] completed tool={}", snap.id, snap.tool_name)
                }
                TaskEvent::Cancelled(snap) => {
                    format!("[task {}] cancelled tool={}", snap.id, snap.tool_name)
                }
                TaskEvent::Failed(snap, error) => {
                    format!(
                        "[task {}] failed   tool={} error={}",
                        snap.id, snap.tool_name, error
                    )
                }
                TaskEvent::ContinueRequested => {
                    "[task] continue requested by background task".to_string()
                }
            };
            let _ = writeln!(io::stderr(), "{line}");
        }
    });

    // --- run ---
    let mut driver = agent
        .start(SessionConfig::new("openrouter-parallel-agent").with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ))
        .await?;

    run_to_completion(&mut driver).await?;

    // If background tasks were detached, wait for them and feed their
    // results back to the model for a follow-up turn.
    idle_handle.wait_for_idle().await;
    run_to_completion(&mut driver).await?;

    Ok(())
}

fn system_item() -> Item {
    Item::text(ItemKind::System, SYSTEM_PROMPT)
}

fn user_item(prompt: &str) -> Item {
    Item::text(ItemKind::User, prompt)
}

async fn run_to_completion<S>(
    driver: &mut agentkit_loop::LoopDriver<S>,
) -> Result<(), Box<dyn std::error::Error>>
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
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                let _ = pending.deny_with_reason(driver, "Tool call rejected, try something else.");
                continue;
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            // Background tasks may still be running — the caller can wait
            // for them and re-enter if needed.
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => return Ok(()),
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
            Part::ToolCall(call) => {
                println!("[tool call] {} {}", call.name, call.input);
            }
            Part::Structured(value) => {
                if !saw_output {
                    println!("[output]");
                    saw_output = true;
                }
                println!("{}", value.value);
            }
            Part::Media(_) | Part::File(_) | Part::ToolResult(_) | Part::Custom(_) => {}
        }
    }
}
