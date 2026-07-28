use openrouter_subagent_tool::run_probe;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let secret = std::env::var("SUBAGENT_SECRET").unwrap_or_else(|_| "LANTERN-SECRET-93B7".into());

    let run = run_probe(
        &secret,
        if prompt.trim().is_empty() {
            None
        } else {
            Some(prompt.as_str())
        },
    )
    .await?;

    println!("[output]");
    println!("{}", run.output.trim());
    eprintln!("[tool calls] {}", run.tool_calls.join(", "));

    Ok(())
}
