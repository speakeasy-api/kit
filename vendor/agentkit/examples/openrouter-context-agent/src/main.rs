use std::env;
use std::io;
use std::path::PathBuf;

use agentkit_context::{AgentsMd, ContextLoader};
use agentkit_core::{Item, ItemKind, Part};
use agentkit_loop::{
    Agent, LoopInterrupt, LoopStep, PromptCacheRequest, PromptCacheRetention, SessionConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use agentkit_reporting::{CompositeReporter, StdoutReporter};
use agentkit_tool_skills::SkillRegistry;

const SYSTEM_PROMPT: &str = "\
You are a careful repository assistant.
Use the loaded context items as authoritative project guidance.
When a task matches a skill's description, activate it before proceeding.
Prefer concise answers.
";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let args = Args::parse()?;

    // Load project-level instructions (AGENTS.md) eagerly — these are small.
    let context_items = ContextLoader::new()
        .with_source(AgentsMd::discover(&args.context_root))
        .load()
        .await?;

    // Discover skills progressively — catalog in tool description, body on demand.
    let skill_registry = SkillRegistry::from_paths(vec![
        args.context_root.join("skills"),
        args.context_root.join(".agents/skills"),
    ])
    .discover_skills()
    .await;

    let config = OpenRouterConfig::from_env()?;
    let adapter = OpenRouterAdapter::new(config)?;
    let reporter =
        CompositeReporter::new().with_observer(StdoutReporter::new(io::stderr()).with_usage(false));

    let mut builder = Agent::builder().model(adapter).observer(reporter);

    // Only add the skill tool if skills were discovered.
    if skill_registry.has_skills() {
        builder = builder.add_tool_source(skill_registry.tool_registry());
    }

    let mut transcript = vec![system_item()];
    transcript.extend(context_items);

    let agent = builder
        .transcript(transcript)
        .input(vec![user_item(&args.prompt)])
        .build()?;

    let mut driver = agent
        .start(SessionConfig::new("openrouter-context-agent").with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ))
        .await?;

    run_to_completion(&mut driver).await
}

struct Args {
    context_root: PathBuf,
    prompt: String,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn std::error::Error>> {
        let mut args = env::args().skip(1);
        let mut context_root: Option<PathBuf> = None;
        let mut prompt_parts = Vec::new();

        while let Some(arg) = args.next() {
            if arg == "--context-root" {
                let value = args.next().ok_or("missing value for --context-root")?;
                context_root = Some(PathBuf::from(value));
                continue;
            }
            prompt_parts.push(arg);
            prompt_parts.extend(args);
            break;
        }

        let prompt = prompt_parts.join(" ");
        if prompt.trim().is_empty() {
            return Err(
                "usage: cargo run -p openrouter-context-agent -- [--context-root <path>] '<prompt>'"
                    .into(),
            );
        }

        Ok(Self {
            context_root: context_root.unwrap_or(env::current_dir()?),
            prompt,
        })
    }
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
