use std::{
    collections::BTreeMap,
    env, fs,
    future::Future,
    io::{self, Write},
    path::{Path, PathBuf},
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use kit::tools::CredentialStorage;
use serde::Deserialize;

#[derive(Parser)]
#[command(version, about = "Coding agent runtime and terminal client")]
struct Cli {
    #[command(flatten)]
    telemetry: TelemetryArgs,
    #[command(flatten)]
    openrouter: OpenRouterArgs,
    #[command(subcommand)]
    command: Command,
}

const OTEL_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
const OTEL_CAPTURE_MESSAGE_CONTENT_ENV: &str = "OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT";
const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";

#[derive(Args)]
struct OpenRouterArgs {
    /// OpenRouter API key (prefer the environment or stored credentials to keep it out of argv).
    #[arg(long, global = true, value_name = "KEY")]
    openrouter_api_key: Option<kit::provider::OpenRouterApiKey>,
}

fn resolve_openrouter_api_key(
    cli: Option<kit::provider::OpenRouterApiKey>,
    env: impl Fn(&str) -> Option<String>,
) -> Option<(
    kit::provider::OpenRouterApiKey,
    kit::provider::OpenRouterApiKeySource,
)> {
    cli.map(|key| (key, kit::provider::OpenRouterApiKeySource::Flag))
        .or_else(|| {
            env(OPENROUTER_API_KEY_ENV)
                .and_then(kit::provider::OpenRouterApiKey::non_empty)
                .map(|key| (key, kit::provider::OpenRouterApiKeySource::Environment))
        })
}

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
    /// Highest-precedence MCP server configuration.
    #[arg(long)]
    mcp_config: Option<PathBuf>,
    /// Resolved config.toml MCP path inherited by built-in Kit children.
    #[arg(long = "internal-mcp-config", hide = true)]
    configured_mcp_config: Option<PathBuf>,
    /// Preserve inherited layered configuration without a configured source.
    #[arg(long = "internal-no-mcp-config", hide = true)]
    no_configured_mcp_config: bool,
    /// Preserve legacy single-file MCP behavior in built-in Kit children.
    #[arg(long = "internal-mcp-legacy", hide = true)]
    legacy_mcp_config: bool,
    #[command(flatten)]
    credentials: CredentialArgs,
}

impl McpArgs {
    fn config_paths(&self, config: &Config) -> io::Result<(Option<PathBuf>, Option<PathBuf>)> {
        fn launch_path(path: &Path) -> io::Result<PathBuf> {
            if path.is_absolute() {
                Ok(path.to_path_buf())
            } else {
                Ok(env::current_dir()?.join(path))
            }
        }

        if self.no_configured_mcp_config && self.configured_mcp_config.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "inherited MCP configuration cannot be both present and absent",
            ));
        }
        let configured = if self.no_configured_mcp_config {
            None
        } else {
            self.configured_mcp_config
                .as_deref()
                .or(config.mcp_config.as_deref())
                .map(launch_path)
                .transpose()?
        };
        Ok((
            configured,
            self.mcp_config.as_deref().map(launch_path).transpose()?,
        ))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, ValueEnum)]
#[serde(rename_all = "lowercase")]
enum CredentialStoreKind {
    Memory,
    Keychain,
    File,
}

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum)]
enum ReasoningEffortArg {
    Default,
    Low,
    Medium,
    High,
}

impl ReasoningEffortArg {
    const fn resolved(self) -> Option<kit::ReasoningEffort> {
        match self {
            Self::Default => None,
            Self::Low => Some(kit::ReasoningEffort::Low),
            Self::Medium => Some(kit::ReasoningEffort::Medium),
            Self::High => Some(kit::ReasoningEffort::High),
        }
    }
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

fn absolute_parent(path: &Path) -> io::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    Ok(path.parent().unwrap_or(Path::new(".")).to_path_buf())
}

fn migrate_config(mut config: toml::Table) -> toml::Table {
    for migration in CONFIG_MIGRATIONS {
        (migration.apply)(&mut config);
    }
    config
}

#[derive(Debug, Default, Deserialize)]
struct Config {
    root: Option<PathBuf>,
    model: Option<String>,
    provider: Option<kit::ProviderKind>,
    reasoning_effort: Option<kit::ReasoningEffort>,
    a2a: Option<String>,
    otel_endpoint: Option<String>,
    otel_capture_message_content: Option<bool>,
    otel_message_content_max_messages: Option<usize>,
    otel_message_content_max_bytes: Option<usize>,
    mcp_config: Option<PathBuf>,
    #[allow(dead_code)]
    #[serde(default)]
    plugins: BTreeMap<String, kit::plugins::PluginConfig>,
    credential_store: Option<CredentialStoreKind>,
    credential_dir: Option<PathBuf>,
    #[serde(default)]
    acp: BTreeMap<String, kit::AcpHarnessProfile>,
    subagent: Option<SubagentConfig>,
    #[serde(skip)]
    config_dir: PathBuf,
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct SubagentConfig {
    harness: String,
    #[serde(default)]
    harnesses: BTreeMap<String, kit::SubagentHarnessPolicy>,
}

impl Config {
    fn load_default() -> io::Result<Self> {
        let Some(home) = env::var_os("HOME").filter(|home| !home.is_empty()) else {
            return Ok(Self::default());
        };
        Self::load(&PathBuf::from(home).join(".kit/config.toml"))
    }

    fn load(path: &Path) -> io::Result<Self> {
        let config_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            env::current_dir()?.join(path)
        };
        let config_dir = absolute_parent(&config_path)?;
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    config_dir,
                    config_path: Some(config_path),
                    ..Self::default()
                });
            }
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
                if config
                    .get("subagent")
                    .and_then(toml::Value::as_table)
                    .is_some_and(|subagent| subagent.contains_key("names"))
                {
                    return Err(
                        "could not parse config: unknown field `names` in `subagent`".into(),
                    );
                }
                toml::Value::Table(config)
                    .try_into()
                    .map_err(|error| format!("could not parse config: {error}"))
            });
        let mut config: Self = parsed.map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid config {}: {error}", path.display()),
            )
        })?;
        config.config_dir = config_dir;
        config.config_path = Some(config_path);
        Ok(config)
    }

    async fn plugin_runtime(
        &self,
        runtime_root: &Path,
    ) -> Result<Option<kit::plugins::PluginRuntime>, String> {
        let Some(config_path) = &self.config_path else {
            return Ok(None);
        };
        Ok(Some(
            kit::plugins::PluginRuntime::load(
                config_path.clone(),
                runtime_root.to_path_buf(),
                self.config_dir.join("plugin-cache"),
                self.config_dir.join("plugin-data"),
            )
            .await?,
        ))
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

    fn reasoning_effort(&self, value: Option<ReasoningEffortArg>) -> Option<kit::ReasoningEffort> {
        value.map_or(self.reasoning_effort, ReasoningEffortArg::resolved)
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
        let policies = self
            .subagent
            .as_ref()
            .map(|value| value.harnesses.clone())
            .unwrap_or_default();
        let harnesses = kit::AcpHarnesses::new(self.acp.clone())?.with_model_policies(policies)?;
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
enum AcpProtocolVersion {
    #[value(name = "1")]
    V1,
    #[value(name = "2")]
    V2,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AuthProvider {
    Openai,
    Openrouter,
    Speakeasy,
}

#[derive(Subcommand)]
enum AuthAction {
    /// Authenticate a model provider in the configured credential store.
    Login { provider: AuthProvider },
    /// Show model-provider authentication status.
    Status { provider: AuthProvider },
    /// Remove model-provider credentials, revoking them when supported.
    Logout {
        provider: AuthProvider,
        /// Remove local credentials without attempting remote revocation.
        #[arg(long)]
        local_only: bool,
    },
}

#[derive(Subcommand)]
enum SessionsAction {
    /// Set or clear a session's custom display name.
    Rename {
        /// Durable session ID.
        session_id: String,
        /// New display name.
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        name: Option<String>,
        /// Clear the custom name and restore the generated title.
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Subcommand)]
enum Command {
    /// Write the recommended configuration to ~/.kit/config.toml.
    Init,
    /// Manage provider authentication without starting a runtime.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
        #[command(flatten)]
        credentials: CredentialArgs,
    },
    /// List or rename durable sessions for a workspace.
    Sessions {
        #[command(subcommand)]
        action: Option<SessionsAction>,
        /// Working directory and project context (defaults to config or `.`).
        #[arg(long, global = true)]
        root: Option<PathBuf>,
    },
    /// Serve ACP on stdio with A2A, remote ACP, or both over HTTP.
    Serve {
        /// Working directory and project context (defaults to config or `.`).
        #[arg(long)]
        root: Option<PathBuf>,
        /// Model name (defaults to config or `gpt-5.4`).
        #[arg(long)]
        model: Option<String>,
        /// Model provider (defaults to config or `openai-subscription`).
        #[arg(long, value_enum)]
        provider: Option<kit::ProviderKind>,
        /// Reasoning effort (defaults to config or provider default).
        #[arg(long, value_enum)]
        reasoning_effort: Option<ReasoningEffortArg>,
        /// HTTP listen address. An available loopback port is selected when omitted.
        #[arg(long, visible_alias = "http")]
        a2a: Option<String>,
        /// Expose ACP over HTTP/SSE and WebSocket at `/acp`.
        #[arg(long)]
        remote_acp: bool,
        /// Do not expose A2A on the HTTP listener.
        #[arg(long, requires = "remote_acp")]
        no_a2a: bool,
        /// Do not serve ACP on stdio. Requires remote ACP over HTTP.
        #[arg(long, requires = "remote_acp")]
        no_stdio: bool,
        /// ACP wire version for stdio (defaults to v1 for compatibility).
        #[arg(long, value_enum, default_value = "1", hide = true)]
        stdio_protocol_version: AcpProtocolVersion,
        /// Require this file's bearer token on every HTTP request.
        #[arg(long, value_name = "PATH")]
        server_credential_file: Option<PathBuf>,
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
        /// ACP wire protocol version.
        #[arg(long, value_enum, default_value = "1")]
        protocol_version: AcpProtocolVersion,
        /// Working directory and project context (defaults to config or `.`).
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, value_enum)]
        provider: Option<kit::ProviderKind>,
        #[arg(long, value_enum)]
        reasoning_effort: Option<ReasoningEffortArg>,
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
        #[arg(long, hide = true, requires = "subagent_parent_name")]
        subagent_parent_id: Option<String>,
        #[arg(long, hide = true, requires = "subagent_parent_id")]
        subagent_parent_name: Option<String>,
    },
    /// Run one persisted prompt, print its answer and session id, then exit.
    Prompt {
        /// Working directory and project context (defaults to config or `.`).
        #[arg(long)]
        root: Option<PathBuf>,
        /// Model name (defaults to config or `gpt-5.4`).
        #[arg(long)]
        model: Option<String>,
        /// Model provider (defaults to config or `openai-subscription`).
        #[arg(long, value_enum)]
        provider: Option<kit::ProviderKind>,
        #[arg(long, value_enum)]
        reasoning_effort: Option<ReasoningEffortArg>,
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
        /// Working directory and project context (defaults to config or `.`).
        #[arg(long)]
        root: Option<PathBuf>,
        /// Model name (defaults to config or `gpt-5.4`).
        #[arg(long)]
        model: Option<String>,
        /// Model provider (defaults to config or `openai-subscription`).
        #[arg(long, value_enum)]
        provider: Option<kit::ProviderKind>,
        #[arg(long, value_enum)]
        reasoning_effort: Option<ReasoningEffortArg>,
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

fn initial_config(home: &Path) -> String {
    let kit_dir = home.join(".kit");
    let mcp_config = toml::Value::String(kit_dir.join("mcp.json").to_string_lossy().into());
    let credential_dir = toml::Value::String(kit_dir.join("credentials").to_string_lossy().into());
    format!(
        "model = \"gpt-5.6-sol\"\nmcp_config = {mcp_config}\ncredential_store = \"file\"\ncredential_dir = {credential_dir}\n\n[plugins]\n"
    )
}

fn format_sessions(entries: &[kit::session::CatalogEntry]) -> String {
    let mut output = String::from("UPDATED\tID\tTITLE\tPREVIEW\n");
    for entry in entries {
        output.push_str(&entry.updated_at_rfc3339());
        output.push('\t');
        output.push_str(&entry.id);
        output.push('\t');
        output.push_str(entry.title.as_deref().unwrap_or("-"));
        output.push('\t');
        output.push_str(entry.preview.as_deref().unwrap_or("-"));
        output.push('\n');
    }
    output
}

fn write_if_missing(path: &Path, contents: &[u8]) -> io::Result<()> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => file.write_all(contents),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn init_config(home: &Path) -> io::Result<PathBuf> {
    let kit_dir = home.join(".kit");
    fs::create_dir_all(&kit_dir)?;
    write_if_missing(&kit_dir.join("mcp.json"), b"{\n  \"mcpServers\": {}\n}\n")?;
    let path = kit_dir.join("config.toml");
    write_if_missing(&path, initial_config(home).as_bytes())?;
    Ok(path)
}

fn init_default_config() -> io::Result<PathBuf> {
    let home = env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| io::Error::other("HOME is not set; cannot initialize global config"))?;
    init_config(Path::new(&home))
}

fn validate_auth_storage(action: &AuthAction, storage: &CredentialStorage) -> io::Result<()> {
    if matches!(action, AuthAction::Login { .. }) && matches!(storage, CredentialStorage::Memory) {
        return Err(io::Error::other(
            "provider login cannot use memory credential storage; select --credential-store file --credential-dir <private-directory> or --credential-store keychain",
        ));
    }
    Ok(())
}

async fn execute_auth(
    action: &AuthAction,
    storage: CredentialStorage,
    openrouter_api_key: Option<(
        kit::provider::OpenRouterApiKey,
        kit::provider::OpenRouterApiKeySource,
    )>,
) -> Result<(), io::Error> {
    enum Execution {
        OpenAi(kit::provider::OpenAiAuthCommand),
        OpenRouter(kit::provider::OpenRouterAuthCommand),
        Speakeasy(kit::provider::SpeakeasyAuthCommand),
    }
    let command = match action {
        AuthAction::Login { provider } => match provider {
            AuthProvider::Openai => Execution::OpenAi(kit::provider::OpenAiAuthCommand::Login),
            AuthProvider::Openrouter => {
                Execution::OpenRouter(kit::provider::OpenRouterAuthCommand::Login)
            }
            AuthProvider::Speakeasy => {
                Execution::Speakeasy(kit::provider::SpeakeasyAuthCommand::Login)
            }
        },
        AuthAction::Status { provider } => match provider {
            AuthProvider::Openai => Execution::OpenAi(kit::provider::OpenAiAuthCommand::Status),
            AuthProvider::Openrouter => {
                Execution::OpenRouter(kit::provider::OpenRouterAuthCommand::Status)
            }
            AuthProvider::Speakeasy => {
                Execution::Speakeasy(kit::provider::SpeakeasyAuthCommand::Status)
            }
        },
        AuthAction::Logout {
            provider,
            local_only,
        } => match provider {
            AuthProvider::Openai => Execution::OpenAi(kit::provider::OpenAiAuthCommand::Logout {
                local_only: *local_only,
            }),
            AuthProvider::Openrouter => {
                Execution::OpenRouter(kit::provider::OpenRouterAuthCommand::Logout {
                    local_only: *local_only,
                })
            }
            AuthProvider::Speakeasy => {
                Execution::Speakeasy(kit::provider::SpeakeasyAuthCommand::Logout {
                    local_only: *local_only,
                })
            }
        },
    };
    let output = tokio::task::spawn_blocking(move || match command {
        Execution::OpenAi(command) => kit::provider::execute_openai_auth(command, &storage),
        Execution::OpenRouter(command) => kit::provider::execute_openrouter_auth(
            command,
            &storage,
            openrouter_api_key
                .as_ref()
                .map(|(key, source)| (key, *source)),
        ),
        Execution::Speakeasy(command) => kit::provider::execute_speakeasy_auth(command, &storage),
    })
    .await
    .map_err(io::Error::other)?
    .map_err(io::Error::other)?;
    print!("{output}");
    Ok(())
}

async fn termination_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

async fn supervise_serve(
    runtime: std::sync::Arc<kit::Runtime>,
    sessions: kit::protocols::acp::SessionRegistry,
    no_stdio: bool,
    stdio_protocol_version: AcpProtocolVersion,
    http: kit::protocols::http::HttpServer,
) -> Result<(), Box<dyn std::error::Error>> {
    supervise_serve_with_trigger(
        runtime,
        sessions,
        no_stdio,
        stdio_protocol_version,
        http,
        termination_signal(),
    )
    .await
}

async fn supervise_serve_with_trigger(
    runtime: std::sync::Arc<kit::Runtime>,
    sessions: kit::protocols::acp::SessionRegistry,
    no_stdio: bool,
    stdio_protocol_version: AcpProtocolVersion,
    mut http: kit::protocols::http::HttpServer,
    termination: impl Future<Output = io::Result<()>>,
) -> Result<(), Box<dyn std::error::Error>> {
    enum Exit {
        Stdio(Result<(), agentkit_acp::AcpRuntimeError>),
        Http(io::Result<()>),
        Signal(io::Result<()>),
    }

    let exit = {
        let stdio_sessions = sessions.clone();
        let stdio = async move {
            if no_stdio {
                std::future::pending::<Result<(), agentkit_acp::AcpRuntimeError>>().await
            } else {
                match stdio_protocol_version {
                    AcpProtocolVersion::V1 => {
                        kit::protocols::acp::serve_with_registry(runtime, stdio_sessions).await
                    }
                    AcpProtocolVersion::V2 => {
                        kit::protocols::acp::v2::serve_with_registry(runtime, stdio_sessions).await
                    }
                }
            }
        };
        tokio::pin!(stdio);
        tokio::pin!(termination);
        tokio::select! {
            result = &mut stdio => Exit::Stdio(result),
            result = http.join() => Exit::Http(result),
            result = &mut termination => Exit::Signal(result),
        }
    };

    match exit {
        Exit::Http(result) => {
            sessions.shutdown().await;
            result?;
            Err(io::Error::other("HTTP server stopped unexpectedly").into())
        }
        Exit::Stdio(result) => {
            http.stop_accepting().await;
            sessions.shutdown().await;
            http.shutdown_connections();
            http.join().await?;
            result?;
            Ok(())
        }
        Exit::Signal(result) => {
            http.stop_accepting().await;
            sessions.shutdown().await;
            http.shutdown_connections();
            http.join().await?;
            result?;
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    if matches!(&cli.command, Command::Init) {
        init_default_config()?;
        println!(
            "Kit {}\n\nlog in with your OpenAI, OpenRouter, or Speakeasy account, or set OPENROUTER_API_KEY to get started",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    let config = Config::load_default()?;
    if let Command::Sessions { action, root } = &cli.command {
        let root = config.root(root.clone());
        match action {
            None => print!("{}", format_sessions(&kit::session::catalog(&root)?)),
            Some(SessionsAction::Rename {
                session_id,
                name,
                clear,
            }) => {
                let display_name = if *clear { None } else { name.as_deref() };
                kit::session::set_display_name(&root, session_id, display_name)?;
                if *clear {
                    println!("Cleared name for session {session_id}");
                } else if let Some(name) = name {
                    println!("Renamed session {session_id} to \"{}\"", name.trim());
                }
            }
        }
        return Ok(());
    }
    let openrouter_api_key =
        resolve_openrouter_api_key(cli.openrouter.openrouter_api_key.clone(), |name| {
            env::var(name).ok()
        });
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
        execute_auth(action, storage, openrouter_api_key.clone()).await?;
        return Ok(());
    }
    match cli.command {
        Command::Init => unreachable!("init returns before loading runtime config"),
        Command::Auth { .. } => unreachable!("auth commands return before loading runtime config"),
        Command::Sessions { .. } => unreachable!("sessions returns before starting a runtime"),
        Command::Serve {
            root,
            model,
            provider,
            reasoning_effort,
            a2a,
            remote_acp,
            no_a2a,
            no_stdio,
            stdio_protocol_version,
            server_credential_file,
            mcp,
            session_id,
            resume,
            force,
        } => {
            let root = config.root(root);
            let model = config.model(model);
            let provider = config.provider(provider);
            let reasoning_effort = config.reasoning_effort(reasoning_effort);
            let a2a = config.a2a(a2a);
            let credential_storage = mcp.credentials.storage(&config)?;
            let (configured_mcp, explicit_mcp) = mcp.config_paths(&config)?;
            let plugins = config.plugin_runtime(&root).await?;
            let runtime = match session_id {
                Some(id) => {
                    kit::Runtime::with_session_provider_credentials_effort_and_openrouter_key(
                        &root,
                        model,
                        provider,
                        kit::runtime::SessionRequest { id, resume, force },
                        credential_storage.clone(),
                        reasoning_effort,
                        openrouter_api_key.as_ref().map(|(key, _)| key.clone()),
                    )?
                }
                None => kit::Runtime::new_with_provider_credentials_effort_and_openrouter_key(
                    &root,
                    model,
                    provider,
                    credential_storage.clone(),
                    reasoning_effort,
                    openrouter_api_key.as_ref().map(|(key, _)| key.clone()),
                )?,
            };
            let runtime = kit::Runtime::with_plugin_runtime(runtime, plugins)?;
            let runtime = kit::Runtime::with_telemetry(runtime, telemetry_settings.clone())?;
            let (harnesses, default_harness) = config.harnesses()?;
            let runtime = kit::Runtime::with_acp_harnesses(runtime, harnesses, default_harness)?;
            let runtime = if mcp.legacy_mcp_config {
                kit::Runtime::with_mcp_config(
                    runtime,
                    explicit_mcp.as_deref(),
                    Vec::new(),
                    true,
                    credential_storage,
                )
                .await?
            } else {
                kit::Runtime::with_mcp_sources(
                    runtime,
                    configured_mcp.as_deref(),
                    explicit_mcp.as_deref(),
                    Vec::new(),
                    true,
                    credential_storage,
                )
                .await?
            };
            let address = a2a.unwrap_or_else(|| "127.0.0.1:0".into());
            let serve_a2a = !no_a2a;
            let sessions = kit::protocols::acp::SessionRegistry::new();
            let http = kit::protocols::http::start_with_registry(
                runtime.clone(),
                address,
                serve_a2a,
                remote_acp,
                server_credential_file.as_deref(),
                sessions.clone(),
            )
            .await?;
            let bound = http.address();
            if serve_a2a {
                eprintln!("A2A listening on {bound}");
            }
            if remote_acp {
                eprintln!("ACP v1/v2 listening on http://{bound}/acp");
                eprintln!("ACP v2 listening on http://{bound}/acp/v2");
            }
            supervise_serve(runtime, sessions, no_stdio, stdio_protocol_version, http).await?;
        }
        Command::Acp {
            protocol_version,
            root,
            model,
            provider,
            reasoning_effort,
            mcp,
            session_id,
            resume,
            force,
            subagent_depth,
            subagent_parent_id,
            subagent_parent_name,
        } => {
            let root = config.root(root);
            let model = config.model(model);
            let provider = config.provider(provider);
            let reasoning_effort = config.reasoning_effort(reasoning_effort);
            let credential_storage = mcp.credentials.storage(&config)?;
            let (configured_mcp, explicit_mcp) = mcp.config_paths(&config)?;
            let plugins = config.plugin_runtime(&root).await?;
            let runtime = match session_id {
                Some(id) => {
                    kit::Runtime::with_session_provider_credentials_effort_and_openrouter_key(
                        &root,
                        model,
                        provider,
                        kit::runtime::SessionRequest { id, resume, force },
                        credential_storage.clone(),
                        reasoning_effort,
                        openrouter_api_key.as_ref().map(|(key, _)| key.clone()),
                    )?
                }
                None => kit::Runtime::new_with_provider_credentials_effort_and_openrouter_key(
                    &root,
                    model,
                    provider,
                    credential_storage.clone(),
                    reasoning_effort,
                    openrouter_api_key.as_ref().map(|(key, _)| key.clone()),
                )?,
            };
            let runtime = kit::Runtime::with_plugin_runtime(runtime, plugins)?;
            let runtime = kit::Runtime::with_telemetry(runtime, telemetry_settings.clone())?;
            let runtime = kit::Runtime::with_depth(runtime, subagent_depth)?;
            let runtime = kit::Runtime::with_subagent_parent_context(
                runtime,
                subagent_parent_id.zip(subagent_parent_name),
            )?;
            let (harnesses, default_harness) = config.harnesses()?;
            let runtime = kit::Runtime::with_acp_harnesses(runtime, harnesses, default_harness)?;
            let runtime = if mcp.legacy_mcp_config {
                kit::Runtime::with_mcp_config(
                    runtime,
                    explicit_mcp.as_deref(),
                    Vec::new(),
                    true,
                    credential_storage,
                )
                .await?
            } else {
                kit::Runtime::with_mcp_sources(
                    runtime,
                    configured_mcp.as_deref(),
                    explicit_mcp.as_deref(),
                    Vec::new(),
                    true,
                    credential_storage,
                )
                .await?
            };
            match protocol_version {
                AcpProtocolVersion::V1 => kit::protocols::acp::serve(runtime).await?,
                AcpProtocolVersion::V2 => kit::protocols::acp::v2::serve(runtime).await?,
            }
        }
        Command::Prompt {
            root,
            model,
            provider,
            reasoning_effort,
            mcp,
            resume,
            force,
            prompt,
        } => {
            let root = config.root(root);
            let model = config.model(model);
            let provider = config.provider(provider);
            let reasoning_effort = config.reasoning_effort(reasoning_effort);
            let credential_storage = mcp.credentials.storage(&config)?;
            let (configured_mcp, explicit_mcp) = mcp.config_paths(&config)?;
            let plugins = config.plugin_runtime(&root).await?;
            let session_id = resume.clone().unwrap_or_else(kit::session::new_id);
            let runtime =
                kit::Runtime::with_session_provider_credentials_effort_and_openrouter_key(
                    &root,
                    model,
                    provider,
                    kit::runtime::SessionRequest {
                        id: session_id.clone(),
                        resume: resume.is_some(),
                        force,
                    },
                    credential_storage.clone(),
                    reasoning_effort,
                    openrouter_api_key.as_ref().map(|(key, _)| key.clone()),
                )?;
            let runtime = kit::Runtime::with_plugin_runtime(runtime, plugins)?;
            let runtime = kit::Runtime::with_telemetry(runtime, telemetry_settings.clone())?;
            let (harnesses, default_harness) = config.harnesses()?;
            let runtime = kit::Runtime::with_acp_harnesses(runtime, harnesses, default_harness)?;
            let runtime = if mcp.legacy_mcp_config {
                kit::Runtime::with_mcp_config(
                    runtime,
                    explicit_mcp.as_deref(),
                    Vec::new(),
                    false,
                    credential_storage,
                )
                .await?
            } else {
                kit::Runtime::with_mcp_sources(
                    runtime,
                    configured_mcp.as_deref(),
                    explicit_mcp.as_deref(),
                    Vec::new(),
                    false,
                    credential_storage,
                )
                .await?
            };
            let output = runtime.run_persistent(prompt).await?;
            println!("{output}");
            println!("session_id: {session_id}");
        }
        Command::Tui {
            root,
            model,
            provider,
            reasoning_effort,
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
            let reasoning_effort = config.reasoning_effort(reasoning_effort);
            let a2a = config.a2a(a2a);
            let credential_storage = mcp.credentials.storage(&config)?;
            let (_, explicit_mcp) = mcp.config_paths(&config)?;
            let _ = config.plugin_runtime(&root).await?;
            kit::tui::run_with_reasoning_effort_and_openrouter_key(
                &root,
                &model,
                provider,
                reasoning_effort,
                a2a.as_deref(),
                explicit_mcp.as_deref(),
                &credential_storage,
                &telemetry_settings,
                openrouter_api_key.as_ref().map(|(key, _)| key),
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
    use std::{fs, io, path::PathBuf, sync::Arc, time::Duration};

    use clap::Parser as _;
    use kit::tools::CredentialStorage;

    use super::{
        AuthAction, AuthProvider, Cli, Command, Config, CredentialArgs, CredentialStoreKind,
        McpArgs, OTEL_CAPTURE_MESSAGE_CONTENT_ENV, ReasoningEffortArg, SessionsAction,
        format_sessions, init_config, resolve_openrouter_api_key, supervise_serve_with_trigger,
        validate_auth_storage,
    };

    #[test]
    fn subagent_names_are_rejected_as_unknown_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "[subagent]\nharness = \"acp.kit\"\nnames = [\"Scout\"]\n",
        )
        .unwrap();

        let error = Config::load(&path).expect_err("removed names key must fail config loading");
        assert!(error.to_string().contains("unknown field `names`"));
    }

    #[tokio::test]
    async fn missing_config_path_can_start_empty_and_reload_plugins_later() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing-config.toml");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.config_path.as_deref(), Some(path.as_path()));
        let plugins = config
            .plugin_runtime(directory.path())
            .await
            .unwrap()
            .expect("an exact missing config path still supports live reload");
        assert!(plugins.snapshot().package_roots.is_empty());
    }

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
reasoning_effort = "medium"
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
        assert_eq!(
            config.reasoning_effort(None),
            Some(kit::ReasoningEffort::Medium)
        );
        assert_eq!(
            config.reasoning_effort(Some(ReasoningEffortArg::High)),
            Some(kit::ReasoningEffort::High)
        );
        assert_eq!(
            config.reasoning_effort(Some(ReasoningEffortArg::Default)),
            None
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
            configured_mcp_config: None,
            no_configured_mcp_config: false,
            legacy_mcp_config: false,
            credentials: CredentialArgs {
                credential_store: None,
                credential_dir: None,
            },
        };
        assert_eq!(
            mcp.config_paths(&config).unwrap(),
            (Some(PathBuf::from("/configured/mcp.json")), None)
        );
        let cli_mcp = McpArgs {
            mcp_config: Some(PathBuf::from("/cli/mcp.json")),
            configured_mcp_config: None,
            no_configured_mcp_config: false,
            legacy_mcp_config: false,
            credentials: CredentialArgs {
                credential_store: None,
                credential_dir: None,
            },
        };
        assert_eq!(
            cli_mcp.config_paths(&config).unwrap(),
            (
                Some(PathBuf::from("/configured/mcp.json")),
                Some(PathBuf::from("/cli/mcp.json")),
            )
        );

        let inherited_mcp = McpArgs {
            mcp_config: None,
            configured_mcp_config: Some(PathBuf::from("/parent/configured.json")),
            no_configured_mcp_config: false,
            legacy_mcp_config: false,
            credentials: CredentialArgs {
                credential_store: None,
                credential_dir: None,
            },
        };
        assert_eq!(
            inherited_mcp.config_paths(&config).unwrap(),
            (Some(PathBuf::from("/parent/configured.json")), None)
        );
        let inherited_without_config = McpArgs {
            mcp_config: None,
            configured_mcp_config: None,
            no_configured_mcp_config: true,
            legacy_mcp_config: false,
            credentials: CredentialArgs {
                credential_store: None,
                credential_dir: None,
            },
        };
        assert_eq!(
            inherited_without_config.config_paths(&config).unwrap(),
            (None, None)
        );
        let conflicting_inherited_mcp = McpArgs {
            configured_mcp_config: Some(PathBuf::from("/parent/configured.json")),
            ..inherited_without_config
        };
        assert_eq!(
            conflicting_inherited_mcp
                .config_paths(&config)
                .unwrap_err()
                .to_string(),
            "inherited MCP configuration cannot be both present and absent"
        );

        let storage = mcp.credentials.storage(&config).unwrap();
        assert_eq!(storage.cli_name(), "file");
        assert_eq!(
            storage.directory(),
            Some(std::path::Path::new("/configured/credentials"))
        );

        let override_mcp = McpArgs {
            mcp_config: None,
            configured_mcp_config: None,
            no_configured_mcp_config: false,
            legacy_mcp_config: false,
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
        assert_eq!(config.reasoning_effort(None), None);
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
    fn plugin_configuration_is_typed_and_forward_compatible() {
        let configured: Config = toml::from_str(&format!(
            r#"
[plugins.local-plugin]
source = "path"
path = "./plugins/local"
future_option = true

[plugins.remote-plugin]
source = "archive"
url = "https://example.com/plugin.tar.gz"
sha256 = "{}"
subdir = "packages/plugin"

[plugins.git-plugin]
source = "git"
url = "https://example.com/repo"
future_option = true
"#,
            "ab".repeat(32)
        ))
        .unwrap();
        assert_eq!(configured.plugins.len(), 3);
        assert!(matches!(
            configured.plugins["local-plugin"],
            kit::plugins::PluginConfig::Path { .. }
        ));
        assert!(matches!(
            configured.plugins["remote-plugin"],
            kit::plugins::PluginConfig::Archive { .. }
        ));
        assert!(matches!(
            configured.plugins["git-plugin"],
            kit::plugins::PluginConfig::Git { rev: None, .. }
        ));
        assert!(Config::default().plugins.is_empty());

        for invalid in [
            "[plugins.bad]\nsource = 'git'\nrev = 'main'",
            "[plugins.bad]\nsource = 'archive'\nurl = 'https://example.com/plugin.zip'",
        ] {
            assert!(toml::from_str::<Config>(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn file_credentials_require_an_explicit_directory() {
        let missing = McpArgs {
            mcp_config: None,
            configured_mcp_config: None,
            no_configured_mcp_config: false,
            legacy_mcp_config: false,
            credentials: CredentialArgs {
                credential_store: Some(CredentialStoreKind::File),
                credential_dir: None,
            },
        };
        assert!(missing.credentials.storage(&Config::default()).is_err());

        let stray = McpArgs {
            mcp_config: None,
            configured_mcp_config: None,
            no_configured_mcp_config: false,
            legacy_mcp_config: false,
            credentials: CredentialArgs {
                credential_store: Some(CredentialStoreKind::Memory),
                credential_dir: Some("credentials".into()),
            },
        };
        assert!(stray.credentials.storage(&Config::default()).is_err());
    }

    #[test]
    fn named_acp_profiles_ignore_unknown_fields_and_remain_selectable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"
future_option = true

[acp.alpha]
command = "agent-a"
args = ["--stdio"]
permissions = "cancel"
env = { TOKEN = "secret" }

[acp.beta]
command = "agent-b"

[subagent]
harness = "acp.beta"
future_option = true

[subagent.harnesses."acp.beta"]
future_option = true
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

        fs::write(
            &path,
            "[acp.bad]\ncommand = 'agent'\npermissions = 'allow'\n",
        )
        .unwrap();
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn sessions_command_accepts_a_workspace_root_and_formats_catalog_rows() {
        let cli = Cli::try_parse_from(["kit", "sessions", "--root", "/tmp/project"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Sessions { action: None, root: Some(root) }
                if root.as_path() == std::path::Path::new("/tmp/project")
        ));

        let output = format_sessions(&[kit::session::CatalogEntry {
            id: "session-1".into(),
            title: Some("OAuth token bug".into()),
            preview: Some("Fix tests in the catalog".into()),
            is_subagent: false,
            updated_at: 0,
        }]);
        assert_eq!(
            output,
            "UPDATED\tID\tTITLE\tPREVIEW\n1970-01-01T00:00:00.000Z\tsession-1\tOAuth token bug\tFix tests in the catalog\n"
        );
    }

    #[test]
    fn sessions_rename_accepts_names_clear_and_root_in_either_position() {
        let named = Cli::try_parse_from([
            "kit",
            "sessions",
            "rename",
            "s-abc123",
            "OAuth token bug",
            "--root",
            "/tmp/project",
        ])
        .unwrap();
        assert!(matches!(
            named.command,
            Command::Sessions {
                action: Some(SessionsAction::Rename {
                    session_id,
                    name: Some(name),
                    clear: false,
                }),
                root: Some(root),
            } if session_id == "s-abc123"
                && name == "OAuth token bug"
                && root == std::path::Path::new("/tmp/project")
        ));

        let cleared = Cli::try_parse_from([
            "kit",
            "sessions",
            "--root",
            "/tmp/project",
            "rename",
            "s-abc123",
            "--clear",
        ])
        .unwrap();
        assert!(matches!(
            cleared.command,
            Command::Sessions {
                action: Some(SessionsAction::Rename {
                    session_id,
                    name: None,
                    clear: true,
                }),
                root: Some(root),
            } if session_id == "s-abc123"
                && root == std::path::Path::new("/tmp/project")
        ));

        assert!(Cli::try_parse_from(["kit", "sessions", "rename", "s-abc123"]).is_err());
        assert!(
            Cli::try_parse_from(["kit", "sessions", "rename", "s-abc123", "name", "--clear",])
                .is_err()
        );
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
    fn openrouter_key_is_global_redacted_and_uses_cli_then_environment_precedence() {
        let cli = Cli::try_parse_from([
            "kit",
            "prompt",
            "hello",
            "--openrouter-api-key",
            "flag-secret",
        ])
        .unwrap();
        assert_eq!(
            format!("{:?}", cli.openrouter.openrouter_api_key),
            "Some(OpenRouterApiKey([REDACTED]))"
        );
        let (key, source) = resolve_openrouter_api_key(cli.openrouter.openrouter_api_key, |_| {
            Some("environment-secret".into())
        })
        .unwrap();
        assert_eq!(key.as_str(), "flag-secret");
        assert_eq!(source, kit::provider::OpenRouterApiKeySource::Flag);

        assert!(
            Cli::try_parse_from(["kit", "prompt", "hello", "--openrouter-api-key", "",]).is_err()
        );
        assert!(resolve_openrouter_api_key(None, |_| Some(String::new())).is_none());

        for command in ["serve", "acp", "tui"] {
            assert!(
                Cli::try_parse_from(["kit", command, "--openrouter-api-key", "secret"]).is_ok()
            );
        }
        assert!(
            Cli::try_parse_from([
                "kit",
                "auth",
                "status",
                "openrouter",
                "--openrouter-api-key",
                "secret",
            ])
            .is_ok()
        );
    }

    #[test]
    fn init_writes_recommended_global_config() {
        let home = tempfile::tempdir().unwrap();
        let path = init_config(home.path()).unwrap();
        let kit_dir = home.path().join(".kit");
        assert_eq!(path, kit_dir.join("config.toml"));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!(
                "model = \"gpt-5.6-sol\"\nmcp_config = \"{}\"\ncredential_store = \"file\"\ncredential_dir = \"{}\"\n\n[plugins]\n",
                kit_dir.join("mcp.json").display(),
                kit_dir.join("credentials").display(),
            )
        );
        let mcp_path = kit_dir.join("mcp.json");
        assert_eq!(
            fs::read_to_string(&mcp_path).unwrap(),
            "{\n  \"mcpServers\": {}\n}\n"
        );
        fs::write(&path, "existing config\n").unwrap();
        fs::write(&mcp_path, "existing mcp config\n").unwrap();
        init_config(home.path()).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "existing config\n");
        assert_eq!(
            fs::read_to_string(mcp_path).unwrap(),
            "existing mcp config\n"
        );
        assert!(Cli::try_parse_from(["kit", "init"]).is_ok());
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
        assert!(Cli::try_parse_from(["kit", "auth", "login", "speakeasy"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "auth", "status", "speakeasy"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "auth", "logout", "speakeasy"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "auth", "logout", "openai", "--local-only"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "auth", "login", "openrouter"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "auth", "status", "openrouter"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "auth", "logout", "openrouter"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "auth", "logout", "--local-only"]).is_err());
        assert!(Cli::try_parse_from(["kit", "tui", "--credential-store", "memory"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "tui", "--mcp-credential-store", "memory"]).is_err());
    }

    #[test]
    fn standalone_provider_login_rejects_memory_storage() {
        for provider in [
            AuthProvider::Openai,
            AuthProvider::Openrouter,
            AuthProvider::Speakeasy,
        ] {
            let login = AuthAction::Login { provider };
            assert!(validate_auth_storage(&login, &CredentialStorage::Memory).is_err());
            assert!(validate_auth_storage(&login, &CredentialStorage::Keychain).is_ok());
        }
    }

    #[test]
    fn serve_selects_remote_protocols_without_changing_stdio_acp() {
        assert!(
            Cli::try_parse_from(["kit", "acp", "--root", ".", "--provider", "openrouter",]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["kit", "acp", "--root", ".", "--provider", "speakeasy",]).is_ok()
        );
        assert!(Cli::try_parse_from(["kit", "acp", "--provider", "unknown"]).is_err());
        assert!(Cli::try_parse_from(["kit", "acp", "--protocol-version", "2"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "acp", "--protocol-version", "3"]).is_err());
        for command in ["serve", "acp", "tui"] {
            assert!(Cli::try_parse_from(["kit", command, "--reasoning-effort", "high"]).is_ok());
        }
        assert!(
            Cli::try_parse_from(["kit", "prompt", "--reasoning-effort", "default", "hello",])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["kit", "acp", "--reasoning-effort", "extreme"]).is_err());
        assert!(Cli::try_parse_from(["kit", "serve", "--no-a2a"]).is_err());
        assert!(Cli::try_parse_from(["kit", "serve", "--no-stdio"]).is_err());
        assert!(Cli::try_parse_from(["kit", "serve", "--remote-acp"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "serve", "--stdio-protocol-version", "2"]).is_ok());
        assert!(Cli::try_parse_from(["kit", "serve", "--stdio-protocol-version", "3"]).is_err());
        assert!(Cli::try_parse_from(["kit", "serve", "--remote-acp", "--no-a2a"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "kit",
                "serve",
                "--remote-acp",
                "--no-a2a",
                "--no-stdio",
                "--http",
                "0.0.0.0:8081",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["kit", "serve", "--http", "127.0.0.1:0"]).is_ok());
        assert!(
            Cli::try_parse_from(["kit", "serve", "--server-credential-file", "token.txt",]).is_ok()
        );
    }

    #[tokio::test]
    async fn injected_shutdown_stops_no_stdio_supervisor() {
        let root = tempfile::tempdir().unwrap();
        let runtime = kit::Runtime::new(root.path(), "gpt-5.4").unwrap();
        let sessions = kit::protocols::acp::SessionRegistry::new();
        let http = kit::protocols::http::start_with_registry(
            Arc::clone(&runtime),
            "127.0.0.1:0".into(),
            true,
            false,
            None,
            sessions.clone(),
        )
        .await
        .unwrap();
        let (trigger, triggered) = tokio::sync::oneshot::channel();
        trigger.send(()).unwrap();

        tokio::time::timeout(
            Duration::from_secs(2),
            supervise_serve_with_trigger(
                runtime,
                sessions,
                true,
                super::AcpProtocolVersion::V1,
                http,
                async move {
                    triggered
                        .await
                        .map_err(|_| io::Error::other("test shutdown trigger dropped"))
                },
            ),
        )
        .await
        .expect("supervisor shutdown timed out")
        .unwrap();
    }

    #[test]
    fn subagent_model_policies_are_scoped_and_validated_per_harness() {
        let configured: Config = toml::from_str(
            r#"
[subagent]
harness = "acp.kit"

[subagent.harnesses."acp.kit"]
allow_model_overrides = ["sonnet"]

[subagent.harnesses."acp.kit".models]
review = "sonnet"
"#,
        )
        .unwrap();
        configured.harnesses().unwrap();
        let policy = &configured.subagent.unwrap().harnesses["acp.kit"];
        assert_eq!(policy.models["review"], "sonnet");
        assert_eq!(
            policy.allow_model_overrides.as_deref(),
            Some(["sonnet".to_string()].as_slice())
        );

        let invalid: Config = toml::from_str(
            r#"
[subagent]
harness = "acp.kit"

[subagent.harnesses."acp.kit"]
allow_model_overrides = ["sonnet"]

[subagent.harnesses."acp.kit".models]
review = "opus"
"#,
        )
        .unwrap();
        assert!(
            invalid
                .harnesses()
                .unwrap_err()
                .contains("not in allow_model_overrides")
        );
    }

    #[test]
    fn acp_accepts_hidden_inherited_context() {
        let cli = Cli::try_parse_from([
            "kit",
            "acp",
            "--internal-mcp-config",
            "/resolved/configured.json",
            "--subagent-parent-id",
            "s-parent",
            "--subagent-parent-name",
            "偵察 🦀",
        ])
        .unwrap();
        let Command::Acp {
            mcp,
            subagent_parent_id,
            subagent_parent_name,
            ..
        } = cli.command
        else {
            panic!("expected acp command");
        };
        assert_eq!(
            mcp.configured_mcp_config.as_deref(),
            Some(std::path::Path::new("/resolved/configured.json"))
        );
        assert_eq!(subagent_parent_id.as_deref(), Some("s-parent"));
        assert_eq!(subagent_parent_name.as_deref(), Some("偵察 🦀"));

        let cli = Cli::try_parse_from(["kit", "acp", "--internal-no-mcp-config"]).unwrap();
        let Command::Acp { mcp, .. } = cli.command else {
            panic!("expected acp command");
        };
        assert!(mcp.no_configured_mcp_config);
        assert!(
            mcp.config_paths(&Config {
                mcp_config: Some("/child/configured.json".into()),
                ..Config::default()
            })
            .unwrap()
            .0
            .is_none()
        );

        let cli = Cli::try_parse_from([
            "kit",
            "acp",
            "--internal-mcp-legacy",
            "--mcp-config",
            "/legacy/explicit.json",
        ])
        .unwrap();
        let Command::Acp { mcp, .. } = cli.command else {
            panic!("expected acp command");
        };
        assert!(mcp.legacy_mcp_config);
        assert_eq!(
            mcp.mcp_config.as_deref(),
            Some(std::path::Path::new("/legacy/explicit.json"))
        );
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
