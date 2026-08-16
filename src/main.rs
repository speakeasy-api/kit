use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use kit::tools::CredentialStorage;
use serde::Deserialize;

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
    /// OAuth credential storage backend (defaults to config or memory).
    #[arg(long, value_enum)]
    mcp_credential_store: Option<CredentialStoreKind>,
    /// Private directory for file-backed OAuth credentials.
    #[arg(long)]
    mcp_credential_dir: Option<PathBuf>,
}

impl McpArgs {
    fn config_path<'a>(&'a self, config: &'a Config) -> Option<&'a Path> {
        self.mcp_config.as_deref().or(config.mcp_config.as_deref())
    }

    fn storage(&self, config: &Config) -> io::Result<CredentialStorage> {
        let kind = self
            .mcp_credential_store
            .or(config.mcp_credential_store)
            .unwrap_or(CredentialStoreKind::Memory);
        let directory = match self.mcp_credential_store {
            Some(CredentialStoreKind::Memory | CredentialStoreKind::Keychain) => {
                self.mcp_credential_dir.as_ref()
            }
            Some(CredentialStoreKind::File) | None => self
                .mcp_credential_dir
                .as_ref()
                .or(config.mcp_credential_dir.as_ref()),
        };
        match (kind, directory) {
            (CredentialStoreKind::Memory, None) => Ok(CredentialStorage::Memory),
            (CredentialStoreKind::Keychain, None) => Ok(CredentialStorage::Keychain),
            (CredentialStoreKind::File, Some(path)) => {
                Ok(CredentialStorage::Filesystem(path.clone()))
            }
            (CredentialStoreKind::File, None) => Err(io::Error::other(
                "mcp_credential_dir is required when mcp_credential_store is file",
            )),
            (_, Some(_)) => Err(io::Error::other(
                "mcp_credential_dir requires mcp_credential_store to be file",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, ValueEnum)]
#[serde(rename_all = "lowercase")]
enum CredentialStoreKind {
    Memory,
    Keychain,
    File,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    root: Option<PathBuf>,
    model: Option<String>,
    a2a: Option<String>,
    mcp_config: Option<PathBuf>,
    mcp_credential_store: Option<CredentialStoreKind>,
    mcp_credential_dir: Option<PathBuf>,
}

impl Config {
    fn load_default() -> io::Result<Self> {
        let Some(home) = env::var_os("HOME").filter(|home| !home.is_empty()) else {
            return Ok(Self::default());
        };
        Self::load(&PathBuf::from(home).join(".kit/config.toml"))
    }

    fn load(path: &Path) -> io::Result<Self> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("could not read config {}: {error}", path.display()),
                ));
            }
        };
        toml::from_str(&contents).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid config {}: {error}", path.display()),
            )
        })
    }

    fn root(&self, value: Option<PathBuf>) -> PathBuf {
        value
            .or_else(|| self.root.clone())
            .unwrap_or_else(|| ".".into())
    }

    fn model(&self, value: Option<String>) -> String {
        value
            .or_else(|| self.model.clone())
            .unwrap_or_else(|| "gpt-5.4".into())
    }

    fn a2a(&self, value: Option<String>) -> Option<String> {
        value.or_else(|| self.a2a.clone())
    }
}

#[derive(Subcommand)]
enum Command {
    /// Serve the Agent Client Protocol on stdio and A2A over HTTP.
    Serve {
        /// Runtime root (defaults to config or `.`).
        #[arg(long)]
        root: Option<PathBuf>,
        /// Model name (defaults to config or `gpt-5.4`).
        #[arg(long)]
        model: Option<String>,
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
        /// Inherited nested-agent depth (used by parent-owned Kit children).
        #[arg(long, default_value_t = 0, hide = true)]
        subagent_depth: usize,
        /// Disable the A2A listener (used by parent-owned Kit children).
        #[arg(long, hide = true)]
        no_a2a: bool,
    },
    /// Run one persisted prompt, print its answer and session id, then exit.
    Prompt {
        /// Runtime root (defaults to config or `.`).
        #[arg(long)]
        root: Option<PathBuf>,
        /// Model name (defaults to config or `gpt-5.4`).
        #[arg(long)]
        model: Option<String>,
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
        /// Runtime root (defaults to config or `.`).
        #[arg(long)]
        root: Option<PathBuf>,
        /// Model name (defaults to config or `gpt-5.4`).
        #[arg(long)]
        model: Option<String>,
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
    let cli = Cli::parse();
    let config = Config::load_default()?;
    match cli.command {
        Command::Serve {
            root,
            model,
            a2a,
            mcp,
            session_id,
            resume,
            force,
            subagent_depth,
            no_a2a,
        } => {
            let root = config.root(root);
            let model = config.model(model);
            let a2a = config.a2a(a2a);
            let credential_storage = mcp.storage(&config)?;
            let runtime = match session_id {
                Some(id) => kit::Runtime::with_session(
                    root,
                    model,
                    kit::runtime::SessionRequest { id, resume, force },
                )?,
                None => kit::Runtime::new(root, model)?,
            };
            let runtime = kit::Runtime::with_depth(runtime, subagent_depth)?;
            let runtime = kit::Runtime::with_mcp_config(
                runtime,
                mcp.config_path(&config),
                true,
                credential_storage,
            )
            .await?;
            // Depth is checked as defense in depth in case an internal caller
            // forgets the hidden --no-a2a child-process flag.
            if !no_a2a && runtime.base_depth() == 0 {
                let address = a2a.unwrap_or_else(|| "127.0.0.1:0".into());
                let bound = kit::protocols::a2a::start(runtime.clone(), address).await?;
                eprintln!("A2A listening on {bound}");
            }
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
            let root = config.root(root);
            let model = config.model(model);
            let credential_storage = mcp.storage(&config)?;
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
                mcp.config_path(&config),
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
            let root = config.root(root);
            let model = config.model(model);
            let a2a = config.a2a(a2a);
            let credential_storage = mcp.storage(&config)?;
            kit::tui::run(
                &root,
                &model,
                a2a.as_deref(),
                mcp.config_path(&config),
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
    use std::{fs, path::PathBuf};

    use super::{Config, CredentialStoreKind, McpArgs};

    #[test]
    fn config_file_supplies_defaults_and_cli_values_win() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"
root = "/configured/root"
model = "configured-model"
a2a = "127.0.0.1:7331"
mcp_config = "/configured/mcp.json"
mcp_credential_store = "file"
mcp_credential_dir = "/configured/credentials"
"#,
        )
        .unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.root(None), PathBuf::from("/configured/root"));
        assert_eq!(
            config.root(Some("/cli/root".into())),
            PathBuf::from("/cli/root")
        );
        assert_eq!(config.model(None), "configured-model");
        assert_eq!(config.model(Some("cli-model".into())), "cli-model");
        assert_eq!(config.a2a(None).as_deref(), Some("127.0.0.1:7331"));

        let mcp = McpArgs {
            mcp_config: None,
            mcp_credential_store: None,
            mcp_credential_dir: None,
        };
        assert_eq!(
            mcp.config_path(&config),
            Some(std::path::Path::new("/configured/mcp.json"))
        );
        let storage = mcp.storage(&config).unwrap();
        assert_eq!(storage.cli_name(), "file");
        assert_eq!(
            storage.directory(),
            Some(std::path::Path::new("/configured/credentials"))
        );

        let override_mcp = McpArgs {
            mcp_config: None,
            mcp_credential_store: Some(CredentialStoreKind::Memory),
            mcp_credential_dir: None,
        };
        let storage = override_mcp.storage(&config).unwrap();
        assert_eq!(storage.cli_name(), "memory");
    }

    #[test]
    fn missing_config_uses_builtin_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::load(&directory.path().join("missing.toml")).unwrap();
        assert_eq!(config.root(None), PathBuf::from("."));
        assert_eq!(config.model(None), "gpt-5.4");
        assert_eq!(config.a2a(None), None);
    }

    #[test]
    fn file_credentials_require_an_explicit_directory() {
        let missing = McpArgs {
            mcp_config: None,
            mcp_credential_store: Some(CredentialStoreKind::File),
            mcp_credential_dir: None,
        };
        assert!(missing.storage(&Config::default()).is_err());

        let stray = McpArgs {
            mcp_config: None,
            mcp_credential_store: Some(CredentialStoreKind::Memory),
            mcp_credential_dir: Some("credentials".into()),
        };
        assert!(stray.storage(&Config::default()).is_err());
    }
}
