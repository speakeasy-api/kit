use std::env;
use std::error::Error;

use openrouter_compaction_agent::{ShowcaseMode, format_item, run_mode};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse()?;

    match args.mode {
        ModeSelection::All => {
            for mode in ShowcaseMode::all() {
                let run = run_mode(mode, args.prompt.as_deref()).await?;
                print_run(&run);
            }
        }
        ModeSelection::Single(mode) => {
            let run = run_mode(mode, args.prompt.as_deref()).await?;
            print_run(&run);
        }
    }

    Ok(())
}

enum ModeSelection {
    All,
    Single(ShowcaseMode),
}

struct Args {
    mode: ModeSelection,
    prompt: Option<String>,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut args = env::args().skip(1);
        let mut mode = ModeSelection::All;
        let mut prompt_parts = Vec::new();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--mode" => {
                    let value = args.next().ok_or("missing value for --mode")?;
                    mode = if value == "all" {
                        ModeSelection::All
                    } else {
                        ModeSelection::Single(value.parse()?)
                    };
                }
                _ => {
                    prompt_parts.push(arg);
                    prompt_parts.extend(args);
                    break;
                }
            }
        }

        Ok(Self {
            mode,
            prompt: (!prompt_parts.is_empty()).then(|| prompt_parts.join(" ")),
        })
    }
}

fn print_run(run: &openrouter_compaction_agent::ShowcaseRun) {
    println!("== {} ==", run.mode);
    println!("prompt: {}", run.prompt);
    println!("seed transcript items: {}", run.seed_transcript.len());
    for item in &run.seed_transcript {
        println!("  {}", format_item(item));
    }

    if run.compaction_events.is_empty() {
        println!("compaction: none");
    } else {
        for event in &run.compaction_events {
            println!(
                "compaction: reason={:?} replaced_items={} metadata={}",
                event.reason,
                event.replaced_items,
                serde_json::Value::Object(
                    event
                        .metadata
                        .clone()
                        .into_iter()
                        .collect::<serde_json::Map<String, serde_json::Value>>()
                )
            );
        }
    }

    println!("final transcript items: {}", run.final_transcript.len());
    for item in &run.final_transcript {
        println!("  {}", format_item(item));
    }

    println!("assistant output: {}", run.output);
    println!();
}
