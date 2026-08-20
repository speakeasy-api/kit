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
    #[command(flatten)]
    telemetry: TelemetryArgs,
    #[command(subcommand)]
    command: Command,
}

const OTEL_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
const OTEL_CAPTURE_MESSAGE_CONTENT_ENV: &str = "OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT";

#[derive(Args)]
struct TelemetryArgs {
    /// OTLP/gRPC collector endpoint for OpenTelemetry trace export.
    #[arg(long, global = true)]
    otel_endpoint: Option<String>,
    /// Capture structured GenAI input and output messages in exported spans.
    #[arg(long, global = true, value_name = "BOOL", action = clap::ArgAction::Set)]
    otel_capture_message_content: Option<bool>,
    /// Maximum captured messages per GenAI input or output attribute.
    #[arg(long, global = true)]
    otel_message_content_max_messages: Option<usize>,
    /// Maximum captured UTF-8 bytes per GenAI input or output attribute.
    #[arg(long, global = true)]
    otel_message_content_max_bytes: Option<usize>,
}

#[derive(Args)]
struct CredentialArgs {
    /// Credential storage backend (defaults to config or memory).
    #[arg(long, value_enum, global = true)]
    credential_store: Option<CredentialStoreKind>,
    /// Private directory for file-backed credentials.
    #[arg(long, global = true)]
    credential_dir: Option<PathBuf>,
}

impl CredentialArgs {
    fn storage(&self, config: &Config) -> io::Result<CredentialStorage> {
        let kind = self
            .credential_store
            .or(config.credential_store)
            .unwrap_or(CredentialStoreKind::Memory);
        let directory = match self.credential_store {
            Some(CredentialStoreKind::Memory | CredentialStoreKind::Keychain) => {
                self.credential_dir.as_ref()
            }
            Some(CredentialStoreKind::File) | None => self
                .credential_dir
                .as_ref()
                .or(config.credential_dir.as_ref()),
        };
        match (kind, directory) {
            (CredentialStoreKind::Memory, None) => Ok(CredentialStorage::Memory),
            (CredentialStoreKind::Keychain, None) => Ok(CredentialStorage::Keychain),
            (CredentialStoreKind::File, Some(path)) => {
                Ok(CredentialStorage::Filesystem(path.clone()))
            }
            (CredentialStoreKind::File, None) => Err(io::Error::other(
                "credential_dir is required when credential_store is file",
            )),
            (_, Some(_)) => Err(io::Error::other(
                "credential_dir requires credential_store to be file",
            )),
        }
    }
}

#[derive(Args)]
struct McpArgs {
    /// Explicit MCP server configuration (never discovered automatically).
    #[arg(long)]
    mcp_config: Option<PathBuf>,
    #[command(flatten)]
    credentials: CredentialArgs,
}

impl McpArgs {
    fn config_path<'a>(&'a self, config: &'a Config) -> Option<&'a Path> {
        self.mcp_config.as_deref().or(config.mcp_config.as_deref())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, ValueEnum)]
#[serde(rename_all = "lowercase")]
enum CredentialStoreKind {
    Memory,
    Keychain,
    File,
}

struct ConfigMigration {
    apply: fn(&mut toml::Table),
}

// Migrations are shape-driven and must be idempotent because hand-written config
// files do not carry a schema version. Add one migration for every schema change
// so typed deserialization only ever sees the latest config shape.
const CONFIG_MIGRATIONS: &[ConfigMigration] = &[ConfigMigration {
    apply: migrate_credentials_to_shared_store,
}];

fn migrate_credentials_to_shared_store(config: &mut toml::Table) {
    for (old, new) in [
        ("mcp_credential_store", "credential_store"),
        ("mcp_credential_dir", "credential_dir"),
    ] {
        if let Some(value) = config.remove(old) {
            config.entry(new).or_insert(value);
        }
    }
}

fn migrate_config(mut config: toml::Table) -> toml::Table {
    for migration in CONFIG_MIGRATIONS {
        (migration.apply)(&mut config);
    }
    config
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    root: Option<PathBuf>,
    model: Option<String>,
    provider: Option<kit::ProviderKind>,
    a2a: Option<String>,
    otel_endpoint: Option<String>,
    otel_capture_message_content: Option<bool>,
    otel_message_content_max_messages: Option<usize>,
    otel_message_content_max_bytes: Option<usize>,
    mcp_config: Option<PathBuf>,
    credential_store: Option<CredentialStoreKind>,
    credential_dir: Option<PathBuf>,
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
        let parsed = toml::from_str(&contents)
            .map_err(|error| format!("could not parse TOML: {error}"))
            .map(migrate_config)
            .and_then(|config| {
                toml::Value::Table(config)
                    .try_into()
                    .map_err(|error| format!("could not parse config: {error}"))
            });
        parsed.map_err(|error| {
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

    fn otel_endpoint(&self, value: Option<String>, environment: Option<String>) -> Option<String> {
        value
            .or_else(|| self.otel_endpoint.clone())
            .or(environment)
            .filter(|endpoint| !endpoint.is_empty())
    }

    fn telemetry_settings(
        &self,
        args: &TelemetryArgs,
        endpoint_environment: Option<String>,
        capture_environment: Option<String>,
    ) -> io::Result<kit::telemetry::Settings> {
        let capture_message_content = if let Some(value) = args.otel_capture_message_content {
            value
        } else if let Some(value) = self.otel_capture_message_content {
            value
        } else {
            capture_environment
                .map(|value| parse_otel_boolean(OTEL_CAPTURE_MESSAGE_CONTENT_ENV, &value))
                .transpose()?
                .unwrap_or(false)
        };
        let max_messages = args
            .otel_message_content_max_messages
            .or(self.otel_message_content_max_messages)
            .unwrap_or(kit::telemetry::DEFAULT_MESSAGE_CONTENT_MAX_MESSAGES);
        let max_bytes = args
            .otel_message_content_max_bytes
            .or(self.otel_message_content_max_bytes)
            .unwrap_or(kit::telemetry::DEFAULT_MESSAGE_CONTENT_MAX_BYTES);
        kit::telemetry::Settings::try_new(
            self.otel_endpoint(args.otel_endpoint.clone(), endpoint_environment),
            capture_message_content,
            max_messages,
            max_bytes,
        )
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
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

fn parse_otel_boolean(name: &str, value: &str) -> io::Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be true or false, got {value:?}"),
        )),
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AuthProvider {
    Openai,
}

#[derive(Subcommand)]
enum AuthAction {
    /// Authenticate a ChatGPT subscription in the configured credential store.
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
        #[command(flatten)]
        credentials: CredentialArgs,
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

fn validate_auth_storage(action: &AuthAction, storage: &CredentialStorage) -> io::Result<()> {
    if matches!(action, AuthAction::Login { .. }) && matches!(storage, CredentialStorage::Memory) {
        return Err(io::Error::other(
            "OpenAI login cannot use memory credential storage; select --credential-store file --credential-dir <private-directory> or --credential-store keychain",
        ));
    }
    Ok(())
}

async fn execute_auth(action: &AuthAction, storage: CredentialStorage) -> Result<(), io::Error> {
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
    let output =
        tokio::task::spawn_blocking(move || kit::provider::execute_openai_auth(command, &storage))
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
    print!("{output}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = Config::load_default()?;
    let telemetry_settings = config.telemetry_settings(
        &cli.telemetry,
        env::var(OTEL_ENDPOINT_ENV).ok(),
        env::var(OTEL_CAPTURE_MESSAGE_CONTENT_ENV).ok(),
    )?;
    let _telemetry = kit::telemetry::init(&telemetry_settings)?;
    if let Command::Auth {
        action,
        credentials,
    } = &cli.command
    {
        let storage = credentials.storage(&config)?;
        validate_auth_storage(action, &storage)?;
        execute_auth(action, storage).await?;
        return Ok(());
    }
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
            let credential_storage = mcp.credentials.storage(&config)?;
            let runtime = match session_id {
                Some(id) => kit::Runtime::with_session_provider_and_credentials(
                    root,
                    model,
                    provider,
                    kit::runtime::SessionRequest { id, resume, force },
                    credential_storage.clone(),
                )?,
                None => kit::Runtime::new_with_provider_and_credentials(
                    root,
                    model,
                    provider,
                    credential_storage.clone(),
                )?,
            };
            let runtime = kit::Runtime::with_telemetry(runtime, telemetry_settings.clone())?;
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
            let credential_storage = mcp.credentials.storage(&config)?;
            let runtime = match session_id {
                Some(id) => kit::Runtime::with_session_provider_and_credentials(
                    root,
                    model,
                    provider,
                    kit::runtime::SessionRequest { id, resume, force },
                    credential_storage.clone(),
                )?,
                None => kit::Runtime::new_with_provider_and_credentials(
                    root,
                    model,
                    provider,
                    credential_storage.clone(),
                )?,
            };
            let runtime = kit::Runtime::with_telemetry(runtime, telemetry_settings.clone())?;
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
            let credential_storage = mcp.credentials.storage(&config)?;
            let session_id = resume.clone().unwrap_or_else(kit::session::new_id);
            let runtime = kit::Runtime::with_session_provider_and_credentials(
                root,
                model,
                provider,
                kit::runtime::SessionRequest {
                    id: session_id.clone(),
                    resume: resume.is_some(),
                    force,
                },
                credential_storage.clone(),
            )?;
            let runtime = kit::Runtime::with_telemetry(runtime, telemetry_settings.clone())?;
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
            let credential_storage = mcp.credentials.storage(&config)?;
            kit::tui::run(
                &root,
                &model,
                provider,
                a2a.as_deref(),
                mcp.config_path(&config),
                &credential_storage,
                &telemetry_settings,
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
    use kit::tools::CredentialStorage;

    use super::{
        AuthAction, AuthProvider, Cli, Config, CredentialArgs, CredentialStoreKind, McpArgs,
        OTEL_CAPTURE_MESSAGE_CONTENT_ENV, validate_auth_storage,
    };

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
otel_endpoint = "http://configured:4317"
otel_capture_message_content = true
otel_message_content_max_messages = 20
otel_message_content_max_bytes = 200
mcp_config = "/configured/mcp.json"
credential_store = "file"
credential_dir = "/configured/credentials"
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
        assert_eq!(
            config.otel_endpoint(None, Some("http://environment:4317".into())),
            Some("http://configured:4317".into())
        );
        assert_eq!(
            config.otel_endpoint(
                Some("http://cli:4317".into()),
                Some("http://environment:4317".into())
            ),
            Some("http://cli:4317".into())
        );
        let cli = Cli::try_parse_from([
            "kit",
            "prompt",
            "--otel-capture-message-content",
            "false",
            "--otel-message-content-max-messages",
            "5",
            "--otel-message-content-max-bytes",
            "100",
            "hello",
        ])
        .unwrap();
        let telemetry = config
            .telemetry_settings(
                &cli.telemetry,
                Some("http://environment:4317".into()),
                Some("invalid-but-overridden".into()),
            )
            .unwrap();
        assert_eq!(
            telemetry.endpoint.as_deref(),
            Some("http://configured:4317")
        );
        assert!(!telemetry.capture_message_content);
        assert_eq!(telemetry.message_content_max_messages, 5);
        assert_eq!(telemetry.message_content_max_bytes, 100);

        let mcp = McpArgs {
            mcp_config: None,
            credentials: CredentialArgs {
                credential_store: None,
                credential_dir: None,
            },
        };
        assert_eq!(
            mcp.config_path(&config),
            Some(std::path::Path::new("/configured/mcp.json"))
        );
        let storage = mcp.credentials.storage(&config).unwrap();
        assert_eq!(storage.cli_name(), "file");
        assert_eq!(
            storage.directory(),
            Some(std::path::Path::new("/configured/credentials"))
        );

        let override_mcp = McpArgs {
            mcp_config: None,
            credentials: CredentialArgs {
                credential_store: Some(CredentialStoreKind::Memory),
                credential_dir: None,
            },
        };
        let storage = override_mcp.credentials.storage(&config).unwrap();
        assert_eq!(storage.cli_name(), "memory");
    }

    #[test]
    fn legacy_mcp_credentials_are_migrated_before_parsing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"
mcp_credential_store = "file"
mcp_credential_dir = "/legacy/credentials"
"#,
        )
        .unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.credential_store, Some(CredentialStoreKind::File));
        assert_eq!(
            config.credential_dir.as_deref(),
            Some(std::path::Path::new("/legacy/credentials"))
        );
    }

    #[test]
    fn current_config_values_win_over_legacy_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"
mcp_credential_store = "file"
credential_store = "keychain"
"#,
        )
        .unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.credential_store, Some(CredentialStoreKind::Keychain));
    }

    #[test]
    fn missing_config_uses_builtin_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::load(&directory.path().join("missing.toml")).unwrap();
        assert_eq!(config.root(None), PathBuf::from("."));
        assert_eq!(config.model(None), "gpt-5.4");
        assert_eq!(config.provider(None), kit::ProviderKind::OpenAiSubscription);
        assert_eq!(config.a2a(None), None);
        assert_eq!(
            config.otel_endpoint(None, Some("http://environment:4317".into())),
            Some("http://environment:4317".into())
        );
    }

    #[test]
    fn endpoint_precedence_allows_each_higher_priority_source_to_disable_export() {
        let environment = Some("http://environment:4317".into());
        let configured: Config =
            toml::from_str("otel_endpoint = 'http://configured:4317'").unwrap();
        assert_eq!(
            configured
                .otel_endpoint(None, environment.clone())
                .as_deref(),
            Some("http://configured:4317")
        );
        assert_eq!(
            configured
                .otel_endpoint(Some("http://cli:4317".into()), environment.clone())
                .as_deref(),
            Some("http://cli:4317")
        );
        assert_eq!(
            Config::default()
                .otel_endpoint(None, environment.clone())
                .as_deref(),
            Some("http://environment:4317")
        );
        let disabled_cli =
            Cli::try_parse_from(["kit", "--otel-endpoint", "", "prompt", "hello"]).unwrap();
        assert_eq!(
            configured
                .telemetry_settings(&disabled_cli.telemetry, environment.clone(), None)
                .unwrap()
                .endpoint,
            None
        );

        let configured_disabled: Config = toml::from_str("otel_endpoint = ''").unwrap();
        assert_eq!(configured_disabled.otel_endpoint(None, environment), None);
        assert_eq!(Config::default().otel_endpoint(None, None), None);
    }

    #[test]
    fn telemetry_environment_is_strict_and_settings_are_bounded() {
        let config = Config::default();
        let cli = Cli::try_parse_from(["kit", "prompt", "hello"]).unwrap();
        let enabled = config
            .telemetry_settings(&cli.telemetry, None, Some("TrUe".into()))
            .unwrap();
        assert!(enabled.capture_message_content);
        let error = config
            .telemetry_settings(&cli.telemetry, None, Some("yes".into()))
            .unwrap_err();
        assert!(error.to_string().contains(OTEL_CAPTURE_MESSAGE_CONTENT_ENV));

        let invalid = Cli::try_parse_from([
            "kit",
            "prompt",
            "--otel-message-content-max-messages",
            "0",
            "hello",
        ])
        .unwrap();
        assert!(
            config
                .telemetry_settings(&invalid.telemetry, None, None)
                .is_err()
        );

        let configured_false: Config =
            toml::from_str("otel_capture_message_content = false").unwrap();
        let disabled = configured_false
            .telemetry_settings(&cli.telemetry, None, Some("true".into()))
            .unwrap();
        assert!(!disabled.capture_message_content);
        assert!(
            Cli::try_parse_from([
                "kit",
                "prompt",
                "--otel-capture-message-content",
                "yes",
                "hello",
            ])
            .is_err()
        );
    }

    #[test]
    fn file_credentials_require_an_explicit_directory() {
        let missing = McpArgs {
            mcp_config: None,
            credentials: CredentialArgs {
                credential_store: Some(CredentialStoreKind::File),
                credential_dir: None,
            },
        };
        assert!(missing.credentials.storage(&Config::default()).is_err());

        let stray = McpArgs {
            mcp_config: None,
            credentials: CredentialArgs {
                credential_store: Some(CredentialStoreKind::Memory),
                credential_dir: Some("credentials".into()),
            },
        };
        assert!(stray.credentials.storage(&Config::default()).is_err());
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
    fn otel_endpoint_is_a_global_command_line_option() {
        let cli = Cli::try_parse_from([
            "kit",
            "prompt",
            "--otel-endpoint",
            "http://collector:4317",
            "--otel-capture-message-content",
            "true",
            "--otel-message-content-max-messages",
            "12",
            "--otel-message-content-max-bytes",
            "4096",
            "hello",
        ])
        .unwrap();
        assert_eq!(
            cli.telemetry.otel_endpoint.as_deref(),
            Some("http://collector:4317")
        );
        assert_eq!(cli.telemetry.otel_capture_message_content, Some(true));
        assert_eq!(cli.telemetry.otel_message_content_max_messages, Some(12));
        assert_eq!(cli.telemetry.otel_message_content_max_bytes, Some(4096));
    }

    #[test]
    fn auth_commands_parse_without_runtime_arguments() {
        assert!(Cli::try_parse_from(["kit", "auth", "login", "openai"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "kit",
                "auth",
                "login",
                "openai",
                "--credential-store",
                "keychain"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["kit", "auth", "status", "openai"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "auth", "logout", "openai", "--local-only"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "auth", "login", "openrouter"]).is_err());
        assert!(Cli::try_parse_from(["kit", "auth", "logout", "--local-only"]).is_err());
        assert!(Cli::try_parse_from(["kit", "tui", "--credential-store", "memory"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "tui", "--mcp-credential-store", "memory"]).is_err());
    }

    #[test]
    fn standalone_openai_login_rejects_memory_storage() {
        let login = AuthAction::Login {
            provider: AuthProvider::Openai,
        };
        assert!(validate_auth_storage(&login, &CredentialStorage::Memory).is_err());
        assert!(validate_auth_storage(&login, &CredentialStorage::Keychain).is_ok());
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
