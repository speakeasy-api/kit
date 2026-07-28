//! Minimal example: a `#[tool]`-defined tool driven by an agent loop.
//!
//! The whole tool is one async function plus an input struct. The
//! `#[tool]` attribute generates a unit struct with the same name as the
//! function and an impl that:
//!
//! - Builds a `ToolSpec` whose `input_schema` is derived from the input
//!   struct via `schemars::JsonSchema`. Description defaults to the
//!   function's first doc-comment line; override with
//!   `#[tool(description = "...")]`.
//! - Decodes `request.input` into the input type via `serde_json` and
//!   calls the user body. Decode errors surface as
//!   `ToolError::InvalidInput` and land in the transcript as an error
//!   `ToolResult` so the model can react.
//!
//! Run with:
//!
//! ```bash
//! cargo run -p openrouter-macro-tool -- "Use word_count to count the words in 'hello agentkit world'"
//! ```

use std::error::Error;

use agentkit_core::{Item, ItemKind, Part, ToolCallId, ToolOutput, ToolResultPart};
use agentkit_loop::{
    Agent, LoopInterrupt, LoopStep, PromptCacheRequest, PromptCacheRetention, SessionConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use agentkit_tools_core::{ToolError, ToolRegistry, ToolResult};
use agentkit_tools_derive::tool;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(JsonSchema, Deserialize)]
struct WordCountInput {
    /// The text whose words should be counted.
    text: String,
}

/// Count the number of whitespace-separated words in a string.
#[tool]
async fn word_count(input: WordCountInput) -> Result<ToolResult, ToolError> {
    let count = input.text.split_whitespace().count();
    Ok(ToolResult::new(ToolResultPart::success(
        ToolCallId::default(),
        ToolOutput::text(format!("{count} word(s) in: {:?}", input.text)),
    )))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();

    let prompt = std::env::args()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    if prompt.is_empty() {
        eprintln!(
            "usage: cargo run -p openrouter-macro-tool -- \"Use word_count to count the words in 'hello world'\""
        );
        std::process::exit(1);
    }

    let config = OpenRouterConfig::from_env()?;
    let adapter = OpenRouterAdapter::new(config)?;

    // Registering a `#[tool]`-defined tool reads exactly the same as a
    // hand-written one — the macro generates a unit struct whose name
    // matches the function.
    let tools = ToolRegistry::new().with(word_count);

    let agent = Agent::builder()
        .model(adapter)
        .add_tool_source(tools)
        .transcript(vec![Item::text(
            ItemKind::System,
            "You are a helper that uses the `word_count` tool when asked to count words.",
        )])
        .input(vec![Item::text(ItemKind::User, prompt)])
        .build()?;

    let mut driver = agent
        .start(SessionConfig::new("macro-tool-demo").with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ))
        .await?;

    loop {
        match driver.next().await? {
            LoopStep::Finished(result) => {
                for item in result.items {
                    if item.kind != ItemKind::Assistant {
                        continue;
                    }
                    for part in item.parts {
                        if let Part::Text(text) = part {
                            println!("{}", text.text);
                        }
                    }
                }
                break;
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {
                println!("tool called");
            }
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                eprintln!("[unexpected: model asked for more input]");
                break;
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                eprintln!(
                    "[unexpected approval request for {}: {}]",
                    pending.request.request_kind, pending.request.summary
                );
                break;
            }
        }
    }

    Ok(())
}
