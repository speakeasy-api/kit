use std::{io, path::PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use kit::tools::CredentialStorage;

#[derive(Parser)]
#[command(version, about = "Lean directory-rooted coding agent runtime")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct McpArgs {
    /// Explicit MCP server configuration (never discovered automatically).
    #[arg(long)]
    mcp_config: Option<PathBuf>,
    /// OAuth credential storage backend.
    #[arg(long, value_enum, default_value_t = CredentialStoreKind::Memory)]
    mcp_credential_store: CredentialStoreKind,
    /// Private directory for file-backed OAuth credentials.
    #[arg(long)]
    mcp_credential_dir: Option<PathBuf>,
}

impl McpArgs {
    fn storage(&self) -> io::Result<CredentialStorage> {
        match (self.mcp_credential_store, &self.mcp_credential_dir) {
            (CredentialStoreKind::Memory, None) => Ok(CredentialStorage::Memory),
            (CredentialStoreKind::Keychain, None) => Ok(CredentialStorage::Keychain),
            (CredentialStoreKind::File, Some(path)) => {
                Ok(CredentialStorage::Filesystem(path.clone()))
            }
            (CredentialStoreKind::File, None) => Err(io::Error::other(
                "--mcp-credential-dir is required with --mcp-credential-store file",
            )),
            (_, Some(_)) => Err(io::Error::other(
                "--mcp-credential-dir requires --mcp-credential-store file",
            )),
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum CredentialStoreKind {
    Memory,
    Keychain,
    File,
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
        #[command(flatten)]
        mcp: McpArgs,
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
        #[command(flatten)]
        mcp: McpArgs,
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
        #[command(flatten)]
        mcp: McpArgs,
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
            mcp,
            session_id,
            resume,
            force,
        } => {
            let credential_storage = mcp.storage()?;
            let runtime = match session_id {
                Some(id) => kit::Runtime::with_session(
                    root,
                    model,
                    kit::runtime::SessionRequest { id, resume, force },
                )?,
                None => kit::Runtime::new(root, model)?,
            };
            let runtime = kit::Runtime::with_mcp_config(
                runtime,
                mcp.mcp_config.as_deref(),
                true,
                credential_storage,
            )
            .await?;
            let address = a2a.unwrap_or_else(|| "127.0.0.1:0".into());
            let bound = kit::protocols::a2a::start(runtime.clone(), address).await?;
            eprintln!("A2A listening on {bound}");
            kit::protocols::acp::serve(runtime).await?;
        }
        Command::Prompt {
            root,
            model,
            mcp,
            resume,
            force,
            prompt,
        } => {
            let credential_storage = mcp.storage()?;
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
            let runtime = kit::Runtime::with_mcp_config(
                runtime,
                mcp.mcp_config.as_deref(),
                false,
                credential_storage,
            )
            .await?;
            let output = runtime.run_persistent(prompt).await?;
            println!("{output}");
            println!("session_id: {session_id}");
        }
        Command::Tui {
            root,
            model,
            a2a,
            mcp,
            resume,
            force,
        } => {
            let credential_storage = mcp.storage()?;
            kit::tui::run(
                &root,
                &model,
                a2a.as_deref(),
                mcp.mcp_config.as_deref(),
                &credential_storage,
                resume.as_deref(),
                force,
            )
            .await?
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CredentialStoreKind, McpArgs};

    #[test]
    fn file_credentials_require_an_explicit_directory() {
        let missing = McpArgs {
            mcp_config: None,
            mcp_credential_store: CredentialStoreKind::File,
            mcp_credential_dir: None,
        };
        assert!(missing.storage().is_err());

        let stray = McpArgs {
            mcp_config: None,
            mcp_credential_store: CredentialStoreKind::Memory,
            mcp_credential_dir: Some("credentials".into()),
        };
        assert!(stray.storage().is_err());
    }
}
