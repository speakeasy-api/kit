use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about = "Lean directory-rooted coding agent runtime")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the Agent Client Protocol on stdio and A2A over HTTP.
    Serve {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value = "gpt-5.4")]
        model: String,
        #[arg(long, default_value = "127.0.0.1:7331")]
        a2a: String,
    },
    /// Start the ACP-backed terminal client.
    Tui {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value = "gpt-5.4")]
        model: String,
        #[arg(long, default_value = "127.0.0.1:7331")]
        a2a: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Command::Serve { root, model, a2a } => {
            let runtime = kit::Runtime::new(root, model)?;
            kit::protocols::a2a::start(runtime.clone(), a2a).await?;
            kit::protocols::acp::serve(runtime).await?;
        }
        Command::Tui { root, model, a2a } => kit::tui::run(&root, &model, &a2a).await?,
    }
    Ok(())
}
