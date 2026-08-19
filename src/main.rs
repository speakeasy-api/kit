use std::{
    collections::BTreeMap,
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
    provider: Option<kit::ProviderKind>,
    a2a: Option<String>,
    mcp_config: Option<PathBuf>,
    mcp_credential_store: Option<CredentialStoreKind>,
    mcp_credential_dir: Option<PathBuf>,
    #[serde(default)]
    acp: BTreeMap<String, kit::AcpHarnessProfile>,
    subagent: Option<SubagentConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentConfig {
    harness: String,
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

    fn provider(&self, value: Option<kit::ProviderKind>) -> kit::ProviderKind {
        value.or(self.provider).unwrap_or_default()
    }

    fn a2a(&self, value: Option<String>) -> Option<String> {
        value.or_else(|| self.a2a.clone())
    }

    fn harnesses(&self) -> Result<(kit::AcpHarnesses, String), String> {
        let harnesses = kit::AcpHarnesses::new(self.acp.clone())?;
        let selected = self
            .subagent
            .as_ref()
            .map(|value| value.harness.clone())
            .unwrap_or_else(|| kit::BUILTIN_HARNESS.into());
        if !harnesses.contains(&selected) {
            return Err(format!("unknown subagent ACP harness {selected:?}"));
        }
        Ok((harnesses, selected))
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AuthProvider {
    Openai,
}

#[derive(Subcommand)]
enum AuthAction {
    /// Authenticate a ChatGPT subscription in the OS credential store.
    Login { provider: AuthProvider },
    /// Show ChatGPT subscription authentication status.
    Status { provider: AuthProvider },
    /// Revoke and remove ChatGPT subscription credentials.
    Logout {
        provider: AuthProvider,
        /// Remove local credentials without revoking the remote refresh token.
        #[arg(long)]
        local_only: bool,
    },
}

#[derive(Subcommand)]
enum Command {
    /// Manage provider authentication without starting a runtime.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Serve the Agent Client Protocol on stdio and A2A over HTTP.
    Serve {
        /// Runtime root (defaults to config or `.`).
        #[arg(long)]
        root: Option<PathBuf>,
        /// Model name (defaults to config or `gpt-5.4`).
        #[arg(long)]
        model: Option<String>,
        /// Model provider (defaults to config or `openai-subscription`).
        #[arg(long, value_enum)]
        provider: Option<kit::ProviderKind>,
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
    /// Serve only the Agent Client Protocol on stdio.
    Acp {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, value_enum)]
        provider: Option<kit::ProviderKind>,
        #[command(flatten)]
        mcp: McpArgs,
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long, requires = "session_id")]
        resume: bool,
        #[arg(long, requires = "resume")]
        force: bool,
        #[arg(long, default_value_t = 0, hide = true)]
        subagent_depth: usize,
    },
    /// Run one persisted prompt, print its answer and session id, then exit.
    Prompt {
        /// Runtime root (defaults to config or `.`).
        #[arg(long)]
        root: Option<PathBuf>,
        /// Model name (defaults to config or `gpt-5.4`).
        #[arg(long)]
        model: Option<String>,
        /// Model provider (defaults to config or `openai-subscription`).
        #[arg(long, value_enum)]
        provider: Option<kit::ProviderKind>,
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
        /// Model provider (defaults to config or `openai-subscription`).
        #[arg(long, value_enum)]
        provider: Option<kit::ProviderKind>,
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

fn execute_auth(action: &AuthAction) -> Result<(), io::Error> {
    let command = match action {
        AuthAction::Login {
            provider: AuthProvider::Openai,
        } => kit::provider::OpenAiAuthCommand::Login,
        AuthAction::Status {
            provider: AuthProvider::Openai,
        } => kit::provider::OpenAiAuthCommand::Status,
        AuthAction::Logout {
            provider: AuthProvider::Openai,
            local_only,
        } => kit::provider::OpenAiAuthCommand::Logout {
            local_only: *local_only,
        },
    };
    print!(
        "{}",
        kit::provider::execute_openai_auth(command).map_err(io::Error::other)?
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    if let Command::Auth { action } = &cli.command {
        execute_auth(action)?;
        return Ok(());
    }
    let config = Config::load_default()?;
    match cli.command {
        Command::Auth { .. } => unreachable!("auth commands return before loading runtime config"),
        Command::Serve {
            root,
            model,
            provider,
            a2a,
            mcp,
            session_id,
            resume,
            force,
        } => {
            let root = config.root(root);
            let model = config.model(model);
            let provider = config.provider(provider);
            let a2a = config.a2a(a2a);
            let credential_storage = mcp.storage(&config)?;
            let runtime = match session_id {
                Some(id) => kit::Runtime::with_session_and_provider(
                    root,
                    model,
                    provider,
                    kit::runtime::SessionRequest { id, resume, force },
                )?,
                None => kit::Runtime::new_with_provider(root, model, provider)?,
            };
            let (harnesses, default_harness) = config.harnesses()?;
            let runtime = kit::Runtime::with_acp_harnesses(runtime, harnesses, default_harness)?;
            let runtime = kit::Runtime::with_mcp_config(
                runtime,
                mcp.config_path(&config),
                true,
                credential_storage,
            )
            .await?;
            let address = a2a.unwrap_or_else(|| "127.0.0.1:0".into());
            let bound = kit::protocols::a2a::start(runtime.clone(), address).await?;
            eprintln!("A2A listening on {bound}");
            kit::protocols::acp::serve(runtime).await?;
        }
        Command::Acp {
            root,
            model,
            provider,
            mcp,
            session_id,
            resume,
            force,
            subagent_depth,
        } => {
            let root = config.root(root);
            let model = config.model(model);
            let provider = config.provider(provider);
            let credential_storage = mcp.storage(&config)?;
            let runtime = match session_id {
                Some(id) => kit::Runtime::with_session_and_provider(
                    root,
                    model,
                    provider,
                    kit::runtime::SessionRequest { id, resume, force },
                )?,
                None => kit::Runtime::new_with_provider(root, model, provider)?,
            };
            let runtime = kit::Runtime::with_depth(runtime, subagent_depth)?;
            let (harnesses, default_harness) = config.harnesses()?;
            let runtime = kit::Runtime::with_acp_harnesses(runtime, harnesses, default_harness)?;
            let runtime = kit::Runtime::with_mcp_config(
                runtime,
                mcp.config_path(&config),
                true,
                credential_storage,
            )
            .await?;
            kit::protocols::acp::serve(runtime).await?;
        }
        Command::Prompt {
            root,
            model,
            provider,
            mcp,
            resume,
            force,
            prompt,
        } => {
            let root = config.root(root);
            let model = config.model(model);
            let provider = config.provider(provider);
            let credential_storage = mcp.storage(&config)?;
            let session_id = resume.clone().unwrap_or_else(kit::session::new_id);
            let runtime = kit::Runtime::with_session_and_provider(
                root,
                model,
                provider,
                kit::runtime::SessionRequest {
                    id: session_id.clone(),
                    resume: resume.is_some(),
                    force,
                },
            )?;
            let (harnesses, default_harness) = config.harnesses()?;
            let runtime = kit::Runtime::with_acp_harnesses(runtime, harnesses, default_harness)?;
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
            provider,
            a2a,
            mcp,
            resume,
            force,
        } => {
            // The TUI child reloads this config, but validate profile names and
            // the selected reference before starting that subprocess.
            let _ = config.harnesses()?;
            let root = config.root(root);
            let model = config.model(model);
            let provider = config.provider(provider);
            let a2a = config.a2a(a2a);
            let credential_storage = mcp.storage(&config)?;
            kit::tui::run(
                &root,
                &model,
                provider,
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

    use clap::Parser as _;

    use super::{Cli, Config, CredentialStoreKind, McpArgs};

    #[test]
    fn config_file_supplies_defaults_and_cli_values_win() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"
root = "/configured/root"
model = "configured-model"
provider = "openrouter"
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
        assert_eq!(config.provider(None), kit::ProviderKind::OpenRouter);
        assert_eq!(
            config.provider(Some(kit::ProviderKind::OpenAiSubscription)),
            kit::ProviderKind::OpenAiSubscription
        );
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
        assert_eq!(config.provider(None), kit::ProviderKind::OpenAiSubscription);
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

    #[test]
    fn named_acp_profiles_are_strict_and_selectable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"
[acp.alpha]
command = "agent-a"
args = ["--stdio"]
permissions = "cancel"
[acp.beta]
command = "agent-b"
[subagent]
harness = "acp.beta"
"#,
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        let (harnesses, selected) = config.harnesses().unwrap();
        assert!(harnesses.contains("acp.alpha"));
        assert!(harnesses.contains("acp.beta"));
        assert!(!harnesses.contains("beta"));
        assert!(harnesses.contains(kit::BUILTIN_HARNESS));
        assert_eq!(selected, "acp.beta");
        assert_eq!(
            config.acp["alpha"].permissions,
            kit::AcpPermissionPolicy::Cancel
        );
        assert_eq!(
            config.acp["beta"].permissions,
            kit::AcpPermissionPolicy::Deny
        );

        fs::write(&path, "[acp.bad]\ncommand = 'agent'\nenv = {}\n").unwrap();
        assert!(Config::load(&path).is_err());

        fs::write(
            &path,
            "[acp.bad]\ncommand = 'agent'\npermissions = 'allow'\n",
        )
        .unwrap();
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn auth_commands_parse_without_runtime_arguments() {
        assert!(Cli::try_parse_from(["kit", "auth", "login", "openai"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "auth", "status", "openai"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "auth", "logout", "openai", "--local-only"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "auth", "login", "openrouter"]).is_err());
        assert!(Cli::try_parse_from(["kit", "auth", "logout", "--local-only"]).is_err());
    }

    #[test]
    fn dedicated_acp_command_exists_and_serve_has_no_no_a2a_escape_hatch() {
        assert!(
            Cli::try_parse_from(["kit", "acp", "--root", ".", "--provider", "openrouter",]).is_ok()
        );
        assert!(Cli::try_parse_from(["kit", "acp", "--provider", "unknown"]).is_err());
        assert!(Cli::try_parse_from(["kit", "serve", "--no-a2a"]).is_err());
    }

    #[test]
    fn acp_kit_is_the_kit_profile_with_or_without_explicit_launch_argv() {
        let config = Config::default();
        let (_, selected) = config.harnesses().unwrap();
        assert_eq!(selected, kit::BUILTIN_HARNESS);

        let mut profiles = std::collections::BTreeMap::new();
        profiles.insert(
            "kit".into(),
            kit::AcpHarnessProfile {
                command: "other".into(),
                args: Vec::new(),
                permissions: kit::AcpPermissionPolicy::Deny,
            },
        );
        let harnesses = kit::AcpHarnesses::new(profiles).unwrap();
        assert!(harnesses.is_kit(kit::BUILTIN_HARNESS));

        let configured: Config = toml::from_str(
            "[acp.kit]\ncommand = 'kit'\nargs = ['acp']\n[subagent]\nharness = 'acp.kit'\n",
        )
        .unwrap();
        let (harnesses, selected) = configured.harnesses().unwrap();
        assert_eq!(selected, "acp.kit");
        assert!(harnesses.is_kit(&selected));
    }
}
