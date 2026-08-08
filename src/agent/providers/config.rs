use std::{
    collections::BTreeMap,
    env, fmt, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use agentkit_provider_anthropic::AnthropicConfig;
use agentkit_provider_ollama::OllamaConfig;
use agentkit_provider_openai::OpenAIConfig;
use agentkit_provider_openrouter::{OpenRouterConfig, ReasoningEffort};
use serde::{Deserialize, Deserializer, Serialize, de::MapAccess};
use zeroize::Zeroize;

use crate::{domain::config::Provider, domain::secret::SecretLease};

const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_PROFILE_NAME_BYTES: usize = 64;

#[derive(Clone, Deserialize, Serialize, Eq, PartialEq)]
#[serde(transparent)]
pub(crate) struct SecretValue(String);

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "provider", deny_unknown_fields)]
pub(crate) enum ProviderProfile {
    #[serde(rename = "openai")]
    OpenAi {
        api_key: SecretValue,
        #[serde(default = "default_openai_model")]
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_completion_tokens: Option<u32>,
    },
    #[serde(rename = "openai-subscription")]
    OpenAiSubscription {
        #[serde(default = "default_openai_subscription_model")]
        model: String,
    },
    #[serde(rename = "anthropic")]
    Anthropic {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key: Option<SecretValue>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth_token: Option<SecretValue>,
        model: String,
        max_tokens: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        beta: Option<String>,
    },
    #[serde(rename = "openrouter")]
    OpenRouter {
        api_key: SecretValue,
        #[serde(default = "default_openrouter_model")]
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_completion_tokens: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        temperature: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
    },
    #[serde(rename = "ollama")]
    Ollama {
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_tokens: Option<u32>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderRegistry {
    current: String,
    #[serde(deserialize_with = "deserialize_profiles")]
    providers: BTreeMap<String, ProviderProfile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    mcp_servers: Vec<crate::protocols::mcp::config::McpServerConfig>,
}

pub(crate) enum ConfiguredProvider {
    OpenAi {
        config: OpenAIConfig,
        credential: Arc<SecretLease>,
    },
    Anthropic {
        config: AnthropicConfig,
        credential: Arc<SecretLease>,
    },
    OpenRouter {
        config: OpenRouterConfig,
        credential: Arc<SecretLease>,
    },
    Ollama(OllamaConfig),
    OpenAiSubscription {
        model: String,
    },
}

#[derive(Debug)]
pub(crate) struct ProviderConfigError(String);

impl ProviderConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ProviderConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProviderConfigError {}

impl ProviderProfile {
    pub fn openai(
        api_key: String,
        model: Option<String>,
        base_url: Option<String>,
        max_completion_tokens: Option<u32>,
    ) -> Result<Self, ProviderConfigError> {
        validate_credential_endpoint(base_url.as_deref())?;
        Ok(Self::OpenAi {
            api_key: SecretValue(api_key),
            model: model.unwrap_or_else(default_openai_model),
            base_url,
            max_completion_tokens,
        })
    }

    pub fn openai_subscription(model: Option<String>) -> Self {
        Self::OpenAiSubscription {
            model: model.unwrap_or_else(default_openai_subscription_model),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn anthropic(
        api_key: Option<String>,
        auth_token: Option<String>,
        model: String,
        max_tokens: u32,
        base_url: Option<String>,
        version: Option<String>,
        beta: Option<String>,
    ) -> Result<Self, ProviderConfigError> {
        validate_credential_endpoint(base_url.as_deref())?;
        Ok(Self::Anthropic {
            api_key: api_key.map(SecretValue),
            auth_token: auth_token.map(SecretValue),
            model,
            max_tokens,
            base_url,
            version,
            beta,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn openrouter(
        api_key: String,
        model: Option<String>,
        base_url: Option<String>,
        app_name: Option<String>,
        site_url: Option<String>,
        max_completion_tokens: Option<u32>,
        temperature: Option<f32>,
        reasoning_effort: Option<String>,
    ) -> Result<Self, ProviderConfigError> {
        validate_credential_endpoint(base_url.as_deref())?;
        Ok(Self::OpenRouter {
            api_key: SecretValue(api_key),
            model: model.unwrap_or_else(default_openrouter_model),
            base_url,
            app_name,
            site_url,
            max_completion_tokens,
            temperature,
            reasoning_effort,
        })
    }

    pub fn ollama(model: String, base_url: Option<String>, max_tokens: Option<u32>) -> Self {
        Self::Ollama {
            model,
            base_url,
            max_tokens,
        }
    }

    pub fn provider_name(&self) -> &'static str {
        match self {
            Self::OpenAi { .. } => "openai",
            Self::OpenAiSubscription { .. } => "openai-subscription",
            Self::Anthropic { .. } => "anthropic",
            Self::OpenRouter { .. } => "openrouter",
            Self::Ollama { .. } => "ollama",
        }
    }

    pub fn provider(&self) -> Provider {
        match self {
            Self::OpenAi { .. } => Provider::OpenAi,
            Self::OpenAiSubscription { .. } => Provider::OpenAi,
            Self::Anthropic { .. } => Provider::Anthropic,
            Self::OpenRouter { .. } => Provider::OpenRouter,
            Self::Ollama { .. } => Provider::Ollama,
        }
    }

    fn validate(&self) -> Result<(), ProviderConfigError> {
        let required = |field: &str, value: &str| {
            if value.is_empty() {
                Err(ProviderConfigError::new(format!(
                    "{field} must not be empty"
                )))
            } else {
                Ok(())
            }
        };
        match self {
            Self::OpenAi {
                api_key,
                model,
                base_url,
                max_completion_tokens,
            } => {
                required("openai api_key", &api_key.0)?;
                required("openai model", model)?;
                if max_completion_tokens == &Some(0) {
                    return Err(ProviderConfigError::new(
                        "openai max_completion_tokens must be greater than zero",
                    ));
                }
                validate_credential_endpoint(base_url.as_deref())
            }
            Self::OpenAiSubscription { model } => {
                required("openai-subscription model", model)?;
                if !super::openai_subscription::supported_model(model) {
                    return Err(ProviderConfigError::new(
                        "openai-subscription model is not in the supported model set",
                    ));
                }
                Ok(())
            }
            Self::Anthropic {
                api_key,
                auth_token,
                model,
                max_tokens,
                base_url,
                ..
            } => {
                if api_key.is_some() == auth_token.is_some() {
                    return Err(ProviderConfigError::new(
                        "anthropic requires exactly one of api_key or auth_token",
                    ));
                }
                if let Some(value) = api_key.as_ref().or(auth_token.as_ref()) {
                    required("anthropic credential", &value.0)?;
                }
                required("anthropic model", model)?;
                if *max_tokens == 0 {
                    return Err(ProviderConfigError::new(
                        "anthropic max_tokens must be greater than zero",
                    ));
                }
                validate_credential_endpoint(base_url.as_deref())
            }
            Self::OpenRouter {
                api_key,
                model,
                base_url,
                ..
            } => {
                required("openrouter api_key", &api_key.0)?;
                required("openrouter model", model)?;
                if let Self::OpenRouter {
                    temperature: Some(temperature),
                    ..
                } = self
                    && !temperature.is_finite()
                {
                    return Err(ProviderConfigError::new(
                        "openrouter temperature must be finite",
                    ));
                }
                validate_credential_endpoint(base_url.as_deref())
            }
            Self::Ollama {
                model, max_tokens, ..
            } => {
                required("ollama model", model)?;
                if max_tokens == &Some(0) {
                    return Err(ProviderConfigError::new(
                        "ollama max_tokens must be greater than zero",
                    ));
                }
                Ok(())
            }
        }
    }

    pub fn configure(&self) -> Result<ConfiguredProvider, ProviderConfigError> {
        self.validate()?;
        Ok(match self {
            Self::OpenAi {
                api_key,
                model,
                base_url,
                max_completion_tokens,
            } => {
                let mut config = OpenAIConfig::new(api_key.0.clone(), model.clone());
                if let Some(base_url) = base_url {
                    config = config.with_base_url(base_url.clone());
                }
                if let Some(maximum) = max_completion_tokens {
                    config = config.with_max_completion_tokens(*maximum);
                }
                ConfiguredProvider::OpenAi {
                    config,
                    credential: lease(api_key),
                }
            }
            Self::OpenAiSubscription { model } => ConfiguredProvider::OpenAiSubscription {
                model: model.clone(),
            },
            Self::Anthropic {
                api_key,
                auth_token,
                model,
                max_tokens,
                base_url,
                version,
                beta,
            } => {
                let credential = api_key.as_ref().or(auth_token.as_ref()).expect("validated");
                let mut config = if let Some(api_key) = api_key {
                    AnthropicConfig::new(api_key.0.clone(), model.clone(), *max_tokens)
                } else {
                    AnthropicConfig::with_auth_token(
                        auth_token.as_ref().expect("validated").0.clone(),
                        model.clone(),
                        *max_tokens,
                    )
                }
                .map_err(|error| ProviderConfigError::new(error.to_string()))?;
                if let Some(base_url) = base_url {
                    config = config.with_base_url(base_url.clone());
                }
                if let Some(version) = version {
                    config = config.with_anthropic_version(version.clone());
                }
                if let Some(beta) = beta {
                    for flag in beta
                        .split(',')
                        .map(str::trim)
                        .filter(|flag| !flag.is_empty())
                    {
                        config = config.with_beta(flag.to_owned());
                    }
                }
                ConfiguredProvider::Anthropic {
                    config,
                    credential: lease(credential),
                }
            }
            Self::OpenRouter {
                api_key,
                model,
                base_url,
                app_name,
                site_url,
                max_completion_tokens,
                temperature,
                reasoning_effort,
            } => {
                let mut config = OpenRouterConfig::new(api_key.0.clone(), model.clone());
                if let Some(value) = base_url {
                    config = config.with_base_url(value.clone());
                }
                if let Some(value) = app_name {
                    config = config.with_app_name(value.clone());
                }
                if let Some(value) = site_url {
                    config = config.with_site_url(value.clone());
                }
                if let Some(value) = max_completion_tokens {
                    config = config.with_max_completion_tokens(*value);
                }
                if let Some(value) = temperature {
                    config = config.with_temperature(*value);
                }
                if let Some(value) = reasoning_effort {
                    config = config.with_reasoning_effort(parse_reasoning_effort(value));
                }
                ConfiguredProvider::OpenRouter {
                    config,
                    credential: lease(api_key),
                }
            }
            Self::Ollama {
                model,
                base_url,
                max_tokens,
            } => {
                let mut config = OllamaConfig::new(model.clone());
                if let Some(base_url) = base_url {
                    config = config.with_base_url(base_url.clone());
                }
                if let Some(maximum) = max_tokens {
                    config = config.with_max_tokens(*maximum);
                }
                ConfiguredProvider::Ollama(config)
            }
        })
    }
}

fn validate_credential_endpoint(base_url: Option<&str>) -> Result<(), ProviderConfigError> {
    let Some(base_url) = base_url else {
        return Ok(());
    };
    let endpoint = url::Url::parse(base_url)
        .map_err(|_| ProviderConfigError::new("credential-bearing endpoint is invalid"))?;
    if endpoint.scheme() != "https"
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(ProviderConfigError::new(
            "credential-bearing endpoint must use HTTPS",
        ));
    }
    Ok(())
}

impl ProviderRegistry {
    pub fn new(name: String, profile: ProviderProfile) -> Result<Self, ProviderConfigError> {
        validate_name(&name)?;
        profile.validate()?;
        Ok(Self {
            current: name.clone(),
            providers: BTreeMap::from([(name, profile)]),
            mcp_servers: Vec::new(),
        })
    }

    pub fn load() -> Result<Option<Self>, ProviderConfigError> {
        Self::load_from(&config_path()?)
    }

    pub fn load_from(path: &Path) -> Result<Option<Self>, ProviderConfigError> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("inspect provider config", path, error)),
        };
        if !metadata.file_type().is_file() {
            return Err(ProviderConfigError::new(format!(
                "provider config {} must be a regular file, not a symlink or special file",
                path.display()
            )));
        }
        if metadata.len() > MAX_CONFIG_BYTES {
            return Err(ProviderConfigError::new(format!(
                "provider config {} exceeds the 64 KiB limit",
                path.display()
            )));
        }
        check_permissions(path, &metadata)?;

        let mut options = fs::OpenOptions::new();
        options.read(true);
        no_follow(&mut options);
        let mut file = options
            .open(path)
            .map_err(|error| io_error("open provider config", path, error))?;
        let opened = file
            .metadata()
            .map_err(|error| io_error("inspect open provider config", path, error))?;
        if !opened.is_file() || !same_file(&metadata, &opened) {
            return Err(ProviderConfigError::new(format!(
                "provider config {} changed while it was being opened",
                path.display()
            )));
        }
        check_permissions(path, &opened)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| io_error("read provider config", path, error))?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            bytes.zeroize();
            return Err(ProviderConfigError::new(format!(
                "provider config {} exceeds the 64 KiB limit",
                path.display()
            )));
        }
        let parsed = serde_json::from_slice::<Self>(&bytes).map_err(|error| {
            ProviderConfigError::new(format!(
                "invalid provider config {}: {error}",
                path.display()
            ))
        });
        bytes.zeroize();
        let registry = parsed?;
        registry.validate()?;
        Ok(Some(registry))
    }

    pub fn write_to(&self, path: &Path) -> Result<(), ProviderConfigError> {
        self.validate()?;
        let parent = path.parent().ok_or_else(|| {
            ProviderConfigError::new(format!("provider config {} has no parent", path.display()))
        })?;
        secure_directory(parent)?;
        let (identity, directory) = open_directory(parent)?;

        let mut bytes = serde_json::to_vec_pretty(self).map_err(|error| {
            ProviderConfigError::new(format!("encode provider config: {error}"))
        })?;
        if bytes.len() as u64 + 1 > MAX_CONFIG_BYTES {
            bytes.zeroize();
            return Err(ProviderConfigError::new(
                "provider config exceeds the 64 KiB limit",
            ));
        }

        let mut random = [0_u8; 8];
        getrandom::fill(&mut random)
            .map_err(|error| ProviderConfigError::new(format!("create config nonce: {error}")))?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                ProviderConfigError::new("provider config filename must be valid UTF-8")
            })?;
        let temporary = parent.join(format!(
            ".{name}.{}-{}.tmp",
            std::process::id(),
            u64::from_ne_bytes(random)
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        secure_create(&mut options);
        let mut file = options
            .open(&temporary)
            .map_err(|error| io_error("create temporary provider config", &temporary, error))?;
        #[cfg(unix)]
        file.set_permissions({
            use std::os::unix::fs::PermissionsExt;
            fs::Permissions::from_mode(0o600)
        })
        .map_err(|error| io_error("secure temporary provider config", &temporary, error))?;
        let result = (|| {
            file.write_all(&bytes)
                .map_err(|error| io_error("write temporary provider config", &temporary, error))?;
            file.write_all(b"\n")
                .map_err(|error| io_error("write temporary provider config", &temporary, error))?;
            file.sync_all()
                .map_err(|error| io_error("sync temporary provider config", &temporary, error))?;
            drop(file);
            let current = fs::symlink_metadata(parent)
                .map_err(|error| io_error("inspect provider config directory", parent, error))?;
            if !current.is_dir() || !same_file(&identity, &current) {
                return Err(ProviderConfigError::new(
                    "provider config directory changed during write",
                ));
            }
            replace_file(&temporary, path)
                .map_err(|error| io_error("publish provider config", path, error))?;
            if let Some(directory) = directory {
                directory
                    .sync_all()
                    .map_err(|error| io_error("sync provider config directory", parent, error))?;
            }
            Ok(())
        })();
        bytes.zeroize();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn current(&self) -> (&str, &ProviderProfile) {
        (
            &self.current,
            self.providers
                .get(&self.current)
                .expect("validated registry"),
        )
    }

    pub fn profiles(&self) -> impl Iterator<Item = (&str, &ProviderProfile)> {
        self.providers
            .iter()
            .map(|(name, profile)| (name.as_str(), profile))
    }

    pub fn mcp_servers(&self) -> &[crate::protocols::mcp::config::McpServerConfig] {
        &self.mcp_servers
    }

    pub fn add(
        &mut self,
        name: String,
        profile: ProviderProfile,
        replace: bool,
    ) -> Result<(), ProviderConfigError> {
        validate_name(&name)?;
        profile.validate()?;
        if self.providers.contains_key(&name) && !replace {
            return Err(ProviderConfigError::new(format!(
                "provider profile {name:?} already exists; pass --replace to replace it"
            )));
        }
        self.providers.insert(name, profile);
        Ok(())
    }

    pub fn use_profile(&mut self, name: &str) -> Result<(), ProviderConfigError> {
        validate_name(name)?;
        if !self.providers.contains_key(name) {
            return Err(ProviderConfigError::new(format!(
                "provider profile {name:?} does not exist"
            )));
        }
        self.current = name.to_owned();
        Ok(())
    }

    fn validate(&self) -> Result<(), ProviderConfigError> {
        validate_name(&self.current)?;
        if self.providers.is_empty() {
            return Err(ProviderConfigError::new(
                "provider registry must contain at least one profile",
            ));
        }
        for (name, profile) in &self.providers {
            validate_name(name)?;
            profile.validate()?;
        }
        if !self.providers.contains_key(&self.current) {
            return Err(ProviderConfigError::new(format!(
                "current provider profile {:?} does not exist",
                self.current
            )));
        }
        let mut server_ids = std::collections::BTreeSet::new();
        for server in &self.mcp_servers {
            server.validate().map_err(ProviderConfigError::new)?;
            if !server_ids.insert(server.id.as_str()) {
                return Err(ProviderConfigError::new(format!(
                    "duplicate MCP server {:?}",
                    server.id
                )));
            }
        }
        Ok(())
    }
}

pub(crate) fn config_path() -> Result<PathBuf, ProviderConfigError> {
    if let Some(path) = env::var_os("KIT_CONFIG_FILE") {
        if path.is_empty() {
            return Err(ProviderConfigError::new(
                "KIT_CONFIG_FILE must not be empty",
            ));
        }
        return Ok(PathBuf::from(path));
    }
    #[cfg(windows)]
    {
        return env::var_os("APPDATA")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .map(|path| path.join("Kit/config.json"))
            .ok_or_else(|| ProviderConfigError::new("APPDATA is not set; set KIT_CONFIG_FILE"));
    }
    #[cfg(not(windows))]
    {
        if let Some(path) = env::var_os("XDG_CONFIG_HOME").filter(|path| !path.is_empty()) {
            return Ok(PathBuf::from(path).join("kit/config.json"));
        }
        env::var_os("HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .map(|path| path.join(".config/kit/config.json"))
            .ok_or_else(|| {
                ProviderConfigError::new(
                    "HOME and XDG_CONFIG_HOME are not set; set KIT_CONFIG_FILE",
                )
            })
    }
}

fn default_openai_model() -> String {
    "gpt-4o".to_owned()
}

fn default_openai_subscription_model() -> String {
    "gpt-5.6-sol".to_owned()
}

fn default_openrouter_model() -> String {
    "openrouter/auto".to_owned()
}

fn lease(value: &SecretValue) -> Arc<SecretLease> {
    Arc::new(SecretLease::new(value.0.as_bytes().to_vec()))
}

fn parse_reasoning_effort(value: &str) -> ReasoningEffort {
    match value {
        "minimal" => ReasoningEffort::Minimal,
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        value => ReasoningEffort::Custom(value.to_owned()),
    }
}

fn validate_name(name: &str) -> Result<(), ProviderConfigError> {
    if name.is_empty()
        || name.len() > MAX_PROFILE_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(ProviderConfigError::new(
            "provider profile names must be 1-64 ASCII letters, digits, '.', '_', or '-'",
        ));
    }
    Ok(())
}

fn deserialize_profiles<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, ProviderProfile>, D::Error>
where
    D: Deserializer<'de>,
{
    struct UniqueProfiles;

    impl<'de> serde::de::Visitor<'de> for UniqueProfiles {
        type Value = BTreeMap<String, ProviderProfile>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object of uniquely named provider profiles")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut profiles = BTreeMap::new();
            while let Some((name, profile)) = map.next_entry::<String, ProviderProfile>()? {
                if profiles.insert(name.clone(), profile).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate provider profile {name:?}"
                    )));
                }
            }
            Ok(profiles)
        }
    }

    deserializer.deserialize_map(UniqueProfiles)
}

fn secure_directory(path: &Path) -> Result<(), ProviderConfigError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(ProviderConfigError::new(format!(
                "provider config directory {} must be a directory, not a symlink",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir_all(path)
            .map_err(|error| io_error("create provider config directory", path, error))?,
        Err(error) => return Err(io_error("inspect provider config directory", path, error)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| io_error("secure provider config directory", path, error))?;
    }
    Ok(())
}

#[cfg(unix)]
fn check_permissions(path: &Path, metadata: &fs::Metadata) -> Result<(), ProviderConfigError> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(ProviderConfigError::new(format!(
            "provider config {} has group/other permissions; run chmod 600 {}",
            path.display(),
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_: &Path, _: &fs::Metadata) -> Result<(), ProviderConfigError> {
    Ok(())
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len()
        && left.created().ok() == right.created().ok()
        && left.modified().ok() == right.modified().ok()
}

#[cfg(unix)]
fn open_directory(path: &Path) -> Result<(fs::Metadata, Option<fs::File>), ProviderConfigError> {
    let directory = fs::File::open(path)
        .map_err(|error| io_error("open provider config directory", path, error))?;
    let metadata = directory
        .metadata()
        .map_err(|error| io_error("inspect provider config directory", path, error))?;
    Ok((metadata, Some(directory)))
}

#[cfg(not(unix))]
fn open_directory(path: &Path) -> Result<(fs::Metadata, Option<fs::File>), ProviderConfigError> {
    fs::symlink_metadata(path)
        .map(|metadata| (metadata, None))
        .map_err(|error| io_error("inspect provider config directory", path, error))
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let from = from
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let to = to
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: both paths are NUL-terminated and remain alive for the call.
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn no_follow(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(windows)]
fn no_follow(options: &mut fs::OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
}

#[cfg(not(any(unix, windows)))]
fn no_follow(_: &mut fs::OpenOptions) {}

#[cfg(unix)]
fn secure_create(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
fn secure_create(_: &mut fs::OpenOptions) {}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> ProviderConfigError {
    ProviderConfigError::new(format!("{action} {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    const CANARY: &str = "provider-config-test-canary";

    fn temporary(label: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "kit-provider-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn secure_write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn strict_registry_rejects_malformed_duplicate_unknown_and_invalid_profiles() {
        for document in [
            br#"{"current":"a","providers":{"a":{"provider":"ollama","model":"m"}}"#.as_slice(),
            br#"{"current":"a","current":"a","providers":{"a":{"provider":"ollama","model":"m"}}}"#,
            br#"{"current":"a","providers":{"a":{"provider":"ollama","model":"m"},"a":{"provider":"ollama","model":"n"}}}"#,
            br#"{"current":"a","providers":{"a":{"provider":"ollama","model":"m","model":"n"}}}"#,
            br#"{"current":"a","providers":{"a":{"provider":"ollama","provider":"ollama","model":"m"}}}"#,
            br#"{"current":"a","providers":{"a":{"provider":"ollama","model":"m"}},"unknown":true}"#,
            br#"{"current":"a","providers":{"a":{"provider":"ollama","model":"m","extra":true}}}"#,
            br#"{"current":"missing","providers":{"a":{"provider":"ollama","model":"m"}}}"#,
            br#"{"current":"a","providers":{"a":{"provider":"anthropic","api_key":"x","auth_token":"y","model":"m","max_tokens":1}}}"#,
            br#"{"current":"a","providers":{"a":{"provider":"anthropic","api_key":"x","model":"m","max_tokens":0}}}"#,
        ] {
            let parsed = serde_json::from_slice::<ProviderRegistry>(document)
                .map_err(|error| ProviderConfigError::new(error.to_string()))
                .and_then(|registry| registry.validate());
            assert!(parsed.is_err(), "accepted {}", String::from_utf8_lossy(document));
        }
        for name in ["", "bad name", "slash/name", &"a".repeat(65)] {
            assert!(
                ProviderRegistry::new(
                    name.to_owned(),
                    ProviderProfile::ollama("m".into(), None, None),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn every_profile_builds_the_expected_concrete_agentkit_config() {
        let openai = ProviderProfile::openai(
            CANARY.into(),
            None,
            Some("https://openai".into()),
            Some(321),
        )
        .unwrap();
        let ConfiguredProvider::OpenAi { config, credential } = openai.configure().unwrap() else {
            panic!("expected openai")
        };
        assert_eq!(config.model, "gpt-4o");
        assert_eq!(config.base_url, "https://openai");
        assert_eq!(config.max_completion_tokens, Some(321));
        assert!(!format!("{credential:?}").contains(CANARY));

        let anthropic = ProviderProfile::anthropic(
            None,
            Some(CANARY.into()),
            "claude-test".into(),
            123,
            Some("https://anthropic".into()),
            Some("test-version".into()),
            Some("one, two".into()),
        )
        .unwrap();
        let ConfiguredProvider::Anthropic { config, credential } = anthropic.configure().unwrap()
        else {
            panic!("expected anthropic")
        };
        assert_eq!(config.model, "claude-test");
        assert_eq!(config.max_tokens, 123);
        assert_eq!(config.base_url, "https://anthropic");
        assert_eq!(config.anthropic_version, "test-version");
        assert_eq!(config.anthropic_beta, ["one", "two"]);
        assert!(config.api_key.is_none());
        assert!(config.auth_token.is_some());
        assert!(!format!("{credential:?}").contains(CANARY));

        let openrouter = ProviderProfile::openrouter(
            CANARY.into(),
            None,
            Some("https://openrouter".into()),
            Some("kit-test".into()),
            Some("https://kit.test".into()),
            Some(456),
            Some(0.25),
            Some("high".into()),
        )
        .unwrap();
        let ConfiguredProvider::OpenRouter { config, credential } = openrouter.configure().unwrap()
        else {
            panic!("expected openrouter")
        };
        assert_eq!(config.model, "openrouter/auto");
        assert_eq!(config.base_url, "https://openrouter");
        assert_eq!(config.app_name.as_deref(), Some("kit-test"));
        assert_eq!(config.site_url.as_deref(), Some("https://kit.test"));
        assert_eq!(config.max_completion_tokens, Some(456));
        assert_eq!(config.temperature, Some(0.25));
        assert!(!format!("{credential:?}").contains(CANARY));

        let ollama = ProviderProfile::ollama("llama-test".into(), None, Some(654));
        let ConfiguredProvider::Ollama(config) = ollama.configure().unwrap() else {
            panic!("expected ollama")
        };
        assert_eq!(config.model, "llama-test");
        assert_eq!(
            config.base_url,
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(config.max_tokens, Some(654));

        for profile in [openai, anthropic, openrouter] {
            assert!(!format!("{profile:?}").contains(CANARY));
        }
    }

    #[test]
    fn credential_bearing_provider_constructors_require_https() {
        assert!(
            ProviderProfile::openai("key".into(), None, Some("http://openai".into()), None)
                .is_err()
        );
        assert!(
            ProviderProfile::anthropic(
                Some("key".into()),
                None,
                "model".into(),
                1,
                Some("http://anthropic".into()),
                None,
                None,
            )
            .is_err()
        );
        assert!(
            ProviderProfile::openrouter(
                "key".into(),
                None,
                Some("http://openrouter".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .is_err()
        );
        assert!(
            ProviderProfile::ollama("model".into(), Some("http://127.0.0.1:11434".into()), None,)
                .configure()
                .is_ok()
        );
    }

    #[test]
    fn multiple_profiles_switch_replace_and_round_trip_atomically() {
        let root = temporary("round-trip");
        let path = root.join("kit/config.json");
        let mut registry = ProviderRegistry::new(
            "local".into(),
            ProviderProfile::ollama("one".into(), None, None),
        )
        .unwrap();
        registry
            .add(
                "local.two".into(),
                ProviderProfile::ollama("two".into(), None, None),
                false,
            )
            .unwrap();
        assert!(
            registry
                .add(
                    "local".into(),
                    ProviderProfile::ollama("replacement".into(), None, None),
                    false,
                )
                .is_err()
        );
        registry
            .add(
                "local".into(),
                ProviderProfile::ollama("replacement".into(), None, None),
                true,
            )
            .unwrap();
        registry.use_profile("local.two").unwrap();
        registry.write_to(&path).unwrap();
        let loaded = ProviderRegistry::load_from(&path).unwrap().unwrap();
        assert_eq!(loaded.current().0, "local.two");
        assert_eq!(loaded.profiles().count(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        assert!(fs::read_dir(path.parent().unwrap()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn load_rejects_oversize_and_non_regular_files() {
        let root = temporary("unsafe-files");
        let path = root.join("config.json");
        secure_write(&path, &vec![b' '; MAX_CONFIG_BYTES as usize + 1]);
        assert!(
            ProviderRegistry::load_from(&path)
                .unwrap_err()
                .to_string()
                .contains("64 KiB")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            secure_write(
                &path,
                br#"{"current":"a","providers":{"a":{"provider":"ollama","model":"m"}}}"#,
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(
                ProviderRegistry::load_from(&path)
                    .unwrap_err()
                    .to_string()
                    .contains("chmod 600")
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let link = root.join("link.json");
            symlink(&path, &link).unwrap();
            assert!(
                ProviderRegistry::load_from(&link)
                    .unwrap_err()
                    .to_string()
                    .contains("regular file")
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writer_rejects_an_oversize_registry_before_publishing() {
        let root = temporary("oversize-write");
        let path = root.join("kit/config.json");
        let registry = ProviderRegistry::new(
            "large".into(),
            ProviderProfile::ollama("m".repeat(MAX_CONFIG_BYTES as usize), None, None),
        )
        .unwrap();
        assert!(
            registry
                .write_to(&path)
                .unwrap_err()
                .to_string()
                .contains("64 KiB")
        );
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn config_contract_loads_typed_mcp_and_rejects_raw_secrets_and_duplicates() {
        use crate::domain::ids::{PrincipalId, ProjectId, WorkspaceId};

        let root = temporary("mcp-contract");
        let path = root.join("config.json");
        let server = serde_json::json!({
            "id": "docs",
            "transport": {"kind": "http", "endpoint": "https://example.com/mcp"},
            "owner": {
                "principal_id": PrincipalId::generate().unwrap(),
                "project_id": ProjectId::generate().unwrap(),
                "workspace_id": WorkspaceId::generate().unwrap()
            },
            "source": "mcp.docs",
            "trust_domain": "example.com",
            "namespace": "docs",
            "version": "1",
            "credential_handle": "env:KIT_MCP_DOCS_TOKEN",
            "credential_scope": {"kind": "project"},
            "egress": {"scheme": "https", "host": "example.com", "port": 443},
            "descriptors": [{
                "kind": "tool",
                "remote": "search",
                "descriptor_digest": format!("sha256:{}", "00".repeat(32)),
                "effect": "network_egress",
                "retry_safety": "idempotent",
                "required_grants": ["network_egress"],
                "auth_scopes": ["docs.read"],
                "availability": "available"
            }]
        });
        let document = serde_json::json!({
            "current": "local",
            "providers": {"local": {"provider": "ollama", "model": "test"}},
            "mcp_servers": [server.clone()]
        });
        secure_write(&path, &serde_json::to_vec(&document).unwrap());
        let loaded = ProviderRegistry::load_from(&path).unwrap().unwrap();
        assert_eq!(loaded.mcp_servers().len(), 1);
        assert_eq!(loaded.mcp_servers()[0].id, "docs");

        let mut implicit_scope = server.clone();
        implicit_scope
            .as_object_mut()
            .unwrap()
            .remove("credential_scope");
        let invalid = serde_json::json!({
            "current": "local",
            "providers": {"local": {"provider": "ollama", "model": "test"}},
            "mcp_servers": [implicit_scope]
        });
        secure_write(&path, &serde_json::to_vec(&invalid).unwrap());
        assert!(ProviderRegistry::load_from(&path).is_err());

        let mut plaintext = server.clone();
        plaintext["transport"]["endpoint"] = serde_json::json!("http://example.com/mcp");
        plaintext["egress"]["scheme"] = serde_json::json!("http");
        plaintext["egress"]["port"] = serde_json::json!(80);
        let invalid = serde_json::json!({
            "current": "local",
            "providers": {"local": {"provider": "ollama", "model": "test"}},
            "mcp_servers": [plaintext]
        });
        secure_write(&path, &serde_json::to_vec(&invalid).unwrap());
        assert!(ProviderRegistry::load_from(&path).is_err());

        let mut stdio = server.clone();
        let stdio_profile = crate::executor::profile::ProfileSpec::isolated(
            crate::executor::profile::TrustTier::TrustedLocal,
            if cfg!(target_os = "windows") {
                crate::executor::profile::Platform::Windows
            } else if cfg!(target_os = "macos") {
                crate::executor::profile::Platform::MacOs
            } else {
                crate::executor::profile::Platform::Linux
            },
            if cfg!(target_arch = "aarch64") {
                crate::executor::profile::Architecture::Aarch64
            } else {
                crate::executor::profile::Architecture::X86_64
            },
            crate::executor::profile::ResourceLimits::new(
                10_000,
                256 * 1024 * 1024,
                16,
                16 * 1024 * 1024,
                64 * 1024 * 1024,
                64 * 1024 * 1024,
                16 * 1024 * 1024,
                30_000,
            ),
        );
        let stdio_profile_digest =
            crate::executor::profile::ExecutorProfile::new(stdio_profile.clone())
                .unwrap()
                .digest()
                .to_string();
        stdio["transport"] = serde_json::json!({
            "kind": "stdio",
            "owned_process_profile": "mcp-docs",
            "argv": [std::env::current_exe().unwrap().to_string_lossy()],
            "profile": stdio_profile,
            "profile_digest": stdio_profile_digest,
            "environment": {
                "MCP_TOKEN": {
                    "handle": "env:KIT_MCP_STDIO_TOKEN",
                    "credential_scope": {"kind": "project"}
                },
                "MCP_AUX_TOKEN": {
                    "handle": "env:KIT_MCP_STDIO_AUX_TOKEN",
                    "credential_scope": {"kind": "project"}
                }
            }
        });
        let object = stdio.as_object_mut().unwrap();
        object.remove("credential_handle");
        object.remove("credential_scope");
        object.remove("egress");
        let valid = serde_json::json!({
            "current": "local",
            "providers": {"local": {"provider": "ollama", "model": "test"}},
            "mcp_servers": [stdio.clone()]
        });
        secure_write(&path, &serde_json::to_vec(&valid).unwrap());
        assert!(ProviderRegistry::load_from(&path).unwrap().is_some());

        let mut unscoped_stdio = stdio.clone();
        unscoped_stdio["transport"]["environment"]["MCP_TOKEN"] =
            serde_json::json!({"handle": "env:KIT_MCP_STDIO_TOKEN"});
        let invalid = serde_json::json!({
            "current": "local",
            "providers": {"local": {"provider": "ollama", "model": "test"}},
            "mcp_servers": [unscoped_stdio]
        });
        secure_write(&path, &serde_json::to_vec(&invalid).unwrap());
        assert!(ProviderRegistry::load_from(&path).is_err());

        let mut arbitrary_handle = server.clone();
        arbitrary_handle["credential_handle"] = serde_json::json!("token:secret");
        let invalid = serde_json::json!({
            "current": "local",
            "providers": {"local": {"provider": "ollama", "model": "test"}},
            "mcp_servers": [arbitrary_handle]
        });
        secure_write(&path, &serde_json::to_vec(&invalid).unwrap());
        assert!(ProviderRegistry::load_from(&path).is_err());

        let mut raw_secret = server.clone();
        raw_secret["credential"] = serde_json::json!("raw-token");
        let invalid = serde_json::json!({
            "current": "local",
            "providers": {"local": {"provider": "ollama", "model": "test"}},
            "mcp_servers": [raw_secret]
        });
        secure_write(&path, &serde_json::to_vec(&invalid).unwrap());
        assert!(ProviderRegistry::load_from(&path).is_err());

        let duplicate = serde_json::json!({
            "current": "local",
            "providers": {"local": {"provider": "ollama", "model": "test"}},
            "mcp_servers": [server.clone(), server]
        });
        secure_write(&path, &serde_json::to_vec(&duplicate).unwrap());
        assert!(ProviderRegistry::load_from(&path).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
