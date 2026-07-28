use std::io::{self, Write};

use agentkit_core::{CancellationController, Item, ItemKind, Part};
use agentkit_loop::{
    Agent, InputRequest, LoopInterrupt, LoopStep, PromptCacheRequest, PromptCacheRetention,
    SessionConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let config = OpenRouterConfig::from_env()?;
    let adapter = OpenRouterAdapter::new(config)?;
    let cancellation = CancellationController::new();

    let args: Vec<String> = std::env::args().skip(1).collect();

    let (first_prompt, run_repl) = if args.is_empty() {
        println!("openrouter-chat");
        println!(
            "Type a prompt and press enter. Use /exit to quit. Press Ctrl-C to cancel the current turn."
        );
        let Some(prompt) = read_prompt()? else {
            return Ok(());
        };
        (prompt, true)
    } else {
        (args.join(" "), false)
    };

    let agent = Agent::builder()
        .model(adapter)
        .cancellation(cancellation.handle())
        .input(vec![Item::text(ItemKind::User, first_prompt)])
        .build()?;

    let mut driver = agent
        .start(SessionConfig::new("openrouter-chat").with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ))
        .await?;

    let mut pending = run_turn(&mut driver, &cancellation).await?;

    if !run_repl {
        return Ok(());
    }

    loop {
        let Some(prompt) = read_prompt()? else {
            break;
        };
        pending.submit(&mut driver, vec![Item::text(ItemKind::User, prompt)])?;
        pending = run_turn(&mut driver, &cancellation).await?;
    }

    Ok(())
}

/// Reads one non-empty prompt from stdin. Returns `Ok(None)` on EOF or on
/// `/exit` / `/quit`; loops on empty lines so the user can hit enter without
/// ending the session.
fn read_prompt() -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut line = String::new();
    loop {
        print!("you> ");
        io::stdout().flush()?;
        line.clear();
        if io::stdin().read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        if prompt == "/exit" || prompt == "/quit" {
            return Ok(None);
        }
        return Ok(Some(prompt.to_string()));
    }
}

async fn run_turn<S>(
    driver: &mut agentkit_loop::LoopDriver<S>,
    cancellation: &CancellationController,
) -> Result<InputRequest, Box<dyn std::error::Error>>
where
    S: agentkit_loop::ModelSession,
{
    let interrupt = cancellation.clone();
    let ctrl_c = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        interrupt.interrupt();
    });

    let request = loop {
        match driver.next().await? {
            LoopStep::Finished(result) => {
                if result.finish_reason == agentkit_core::FinishReason::Cancelled {
                    eprintln!("turn cancelled");
                }
                for item in result.items {
                    if item.kind != ItemKind::Assistant {
                        continue;
                    }
                    render_assistant_item(item.parts);
                }
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(req)) => break req,
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                ctrl_c.abort();
                eprintln!("\n[approval required] {}", pending.request.summary);
                return Err("approval interrupt unhandled in openrouter-chat".into());
            }
        }
    };
    ctrl_c.abort();
    Ok(request)
}

fn render_assistant_item(parts: Vec<Part>) {
    let mut reasoning_sections = Vec::new();
    let mut output_sections = Vec::new();
    let mut side_channels = Vec::new();

    for part in parts {
        match part {
            Part::Text(text) => output_sections.push(text.text),
            Part::Reasoning(reasoning) => {
                if let Some(summary) = reasoning.summary {
                    reasoning_sections.push(summary);
                }
            }
            Part::ToolCall(call) => {
                side_channels.push(format!("[tool call] {} {}", call.name, call.input));
            }
            other => {
                side_channels.push(format!("[part {}]", part_kind_name(&other)));
            }
        }
    }

    println!("assistant>");

    if !reasoning_sections.is_empty() {
        println!("[reasoning]");
        for section in &reasoning_sections {
            println!("{section}");
        }
        println!();
    }

    if !output_sections.is_empty() {
        println!("[output]");
        for section in &output_sections {
            println!("{section}");
        }
    }

    if !side_channels.is_empty() {
        if !output_sections.is_empty() || !reasoning_sections.is_empty() {
            println!();
        }
        for line in side_channels {
            println!("{line}");
        }
    }
}

fn part_kind_name(part: &Part) -> &'static str {
    match part {
        Part::Text(_) => "text",
        Part::Media(_) => "media",
        Part::File(_) => "file",
        Part::Structured(_) => "structured",
        Part::Reasoning(_) => "reasoning",
        Part::ToolCall(_) => "tool_call",
        Part::ToolResult(_) => "tool_result",
        Part::Custom(_) => "custom",
    }
}
