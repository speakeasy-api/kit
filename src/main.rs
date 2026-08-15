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
        /// A2A listen address. An available loopback port is selected when omitted.
        #[arg(long)]
        a2a: Option<String>,
        /// Explicit MCP server configuration (never discovered automatically).
        #[arg(long)]
        mcp_config: Option<PathBuf>,
        /// Persistent session id selected by the hosting client.
        #[arg(long)]
        session_id: Option<String>,
        /// Load session_id instead of creating it.
        #[arg(long, requires = "session_id")]
        resume: bool,
        /// Override a stale session lock.
        #[arg(long, requires = "resume")]
        force: bool,
    },
    /// Run one persisted prompt, print its answer and session id, then exit.
    Prompt {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value = "gpt-5.4")]
        model: String,
        /// Explicit MCP server configuration (never discovered automatically).
        #[arg(long)]
        mcp_config: Option<PathBuf>,
        /// Resume this persisted session id.
        #[arg(long)]
        resume: Option<String>,
        /// Override the resumed session's stale lock.
        #[arg(long, requires = "resume")]
        force: bool,
        /// Prompt text; quote it when it contains spaces.
        prompt: String,
    },
    /// Start the ACP-backed terminal client.
    Tui {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value = "gpt-5.4")]
        model: String,
        /// A2A listen address. An available loopback port is selected when omitted.
        #[arg(long)]
        a2a: Option<String>,
        /// Explicit MCP server configuration (never discovered automatically).
        #[arg(long)]
        mcp_config: Option<PathBuf>,
        /// Resume this persisted session id.
        #[arg(long)]
        resume: Option<String>,
        /// Override the resumed session's stale lock.
        #[arg(long, requires = "resume")]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Command::Serve {
            root,
            model,
            a2a,
            mcp_config,
            session_id,
            resume,
            force,
        } => {
            let runtime = match session_id {
                Some(id) => kit::Runtime::with_session(
                    root,
                    model,
                    kit::runtime::SessionRequest { id, resume, force },
                )?,
                None => kit::Runtime::new(root, model)?,
            };
            let runtime = kit::Runtime::with_mcp_config(runtime, mcp_config.as_deref()).await?;
            let address = a2a.unwrap_or_else(|| "127.0.0.1:0".into());
            let bound = kit::protocols::a2a::start(runtime.clone(), address).await?;
            eprintln!("A2A listening on {bound}");
            kit::protocols::acp::serve(runtime).await?;
        }
        Command::Prompt {
            root,
            model,
            mcp_config,
            resume,
            force,
            prompt,
        } => {
            let session_id = resume.clone().unwrap_or_else(kit::session::new_id);
            let runtime = kit::Runtime::with_session(
                root,
                model,
                kit::runtime::SessionRequest {
                    id: session_id.clone(),
                    resume: resume.is_some(),
                    force,
                },
            )?;
            let runtime = kit::Runtime::with_mcp_config(runtime, mcp_config.as_deref()).await?;
            let output = runtime.run_persistent(prompt).await?;
            println!("{output}");
            println!("session_id: {session_id}");
        }
        Command::Tui {
            root,
            model,
            a2a,
            mcp_config,
            resume,
            force,
        } => {
            kit::tui::run(
                &root,
                &model,
                a2a.as_deref(),
                mcp_config.as_deref(),
                resume.as_deref(),
                force,
            )
            .await?
        }
    }
    Ok(())
}
