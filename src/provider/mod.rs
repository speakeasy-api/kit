mod adapter;
mod cerebras;
mod cerebras_auth;
pub mod chatgpt;
mod openai_auth;
mod openrouter_auth;
mod speakeasy_auth;

pub use adapter::{
    KitAdapter, KitSession, ModelGroup, ModelSelection, ProviderKind, ReasoningEffort,
    SelectableAdapter, SelectableSession, model_catalog,
};

/// An Cerebras API key that is redacted in diagnostics and zeroized on drop.
#[derive(Clone, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
pub struct CerebrasApiKey(String);

impl CerebrasApiKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn non_empty(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty()).then(|| Self::new(value))
    }
}

impl std::str::FromStr for CerebrasApiKey {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::non_empty(value.to_owned()).ok_or("Cerebras API key cannot be empty")
    }
}

impl std::fmt::Debug for CerebrasApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CerebrasApiKey([REDACTED])")
    }
}

/// An OpenRouter API key that is redacted in diagnostics and zeroized on drop.
#[derive(Clone, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
pub struct OpenRouterApiKey(String);

impl OpenRouterApiKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn non_empty(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty()).then(|| Self::new(value))
    }
}

impl std::str::FromStr for OpenRouterApiKey {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::non_empty(value.to_owned()).ok_or("OpenRouter API key cannot be empty")
    }
}

impl std::fmt::Debug for OpenRouterApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OpenRouterApiKey([REDACTED])")
    }
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenRouterApiKeySource {
    Flag,
    Environment,
}

impl OpenRouterApiKeySource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Flag => "--openrouter-api-key",
            Self::Environment => "OPENROUTER_API_KEY",
        }
    }
}
pub use chatgpt::{
    OpenAiSubscriptionAdapter, OpenAiSubscriptionSession, OpenAiSubscriptionTurn,
    SubscriptionConfig,
};

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenAiAuthCommand {
    Login,
    Status,
    Logout { local_only: bool },
}

#[doc(hidden)]
pub fn execute_openai_auth(
    command: OpenAiAuthCommand,
    storage: &crate::credentials::CredentialStorage,
) -> Result<String, String> {
    let command = match command {
        OpenAiAuthCommand::Login => openai_auth::AuthCommand::Login,
        OpenAiAuthCommand::Status => openai_auth::AuthCommand::Status,
        OpenAiAuthCommand::Logout { local_only } => openai_auth::AuthCommand::Logout { local_only },
    };
    let timeout = match command {
        openai_auth::AuthCommand::Login => std::time::Duration::from_secs(300),
        openai_auth::AuthCommand::Status | openai_auth::AuthCommand::Logout { .. } => {
            std::time::Duration::from_secs(30)
        }
    };
    openai_auth::execute(command, storage, openai_auth::OutputFormat::Human, timeout)
        .map(|output| output.stdout)
        .map_err(|error| error.to_string())
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenRouterAuthCommand {
    Login,
    Status,
    Logout { local_only: bool },
}

#[doc(hidden)]
pub fn execute_openrouter_auth(
    command: OpenRouterAuthCommand,
    storage: &crate::credentials::CredentialStorage,
    active_key: Option<(&OpenRouterApiKey, OpenRouterApiKeySource)>,
) -> Result<String, String> {
    let command = match command {
        OpenRouterAuthCommand::Login => openrouter_auth::AuthCommand::Login,
        OpenRouterAuthCommand::Status => openrouter_auth::AuthCommand::Status,
        OpenRouterAuthCommand::Logout { local_only } => {
            openrouter_auth::AuthCommand::Logout { local_only }
        }
    };
    let timeout = match command {
        openrouter_auth::AuthCommand::Login => std::time::Duration::from_secs(300),
        openrouter_auth::AuthCommand::Status | openrouter_auth::AuthCommand::Logout { .. } => {
            std::time::Duration::from_secs(30)
        }
    };
    openrouter_auth::execute(
        command,
        storage,
        active_key.map(|(_, source)| source),
        timeout,
    )
}

#[cfg(test)]
pub(crate) fn store_openrouter_test_credentials(storage: &crate::credentials::CredentialStorage) {
    openrouter_auth::store_test_credentials(storage);
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpeakeasyAuthCommand {
    Login,
    Status,
    Logout { local_only: bool },
}

#[doc(hidden)]
pub fn execute_speakeasy_auth(
    command: SpeakeasyAuthCommand,
    storage: &crate::credentials::CredentialStorage,
) -> Result<String, String> {
    let command = match command {
        SpeakeasyAuthCommand::Login => speakeasy_auth::AuthCommand::Login,
        SpeakeasyAuthCommand::Status => speakeasy_auth::AuthCommand::Status,
        SpeakeasyAuthCommand::Logout { local_only } => {
            speakeasy_auth::AuthCommand::Logout { local_only }
        }
    };
    let timeout = match command {
        speakeasy_auth::AuthCommand::Login => std::time::Duration::from_secs(300),
        speakeasy_auth::AuthCommand::Status | speakeasy_auth::AuthCommand::Logout { .. } => {
            std::time::Duration::from_secs(30)
        }
    };
    speakeasy_auth::execute(command, storage, timeout)
}

/// Origin of an explicitly supplied Cerebras API key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CerebrasApiKeySource {
    Flag,
    Environment,
}

impl CerebrasApiKeySource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Flag => "--cerebras-api-key",
            Self::Environment => "CEREBRAS_API_KEY",
        }
    }
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CerebrasAuthCommand {
    Login,
    Status,
    Logout { local_only: bool },
}

/// Load the stored Cerebras key without consulting process environment variables.
pub fn cerebras_api_key(
    storage: &crate::credentials::CredentialStorage,
) -> Result<Option<CerebrasApiKey>, String> {
    cerebras_auth::load(storage)
}

/// Manage a stored key. Login requires an explicitly supplied key; it does not
/// perform remote validation. Logout removes only the local credential.
#[doc(hidden)]
pub fn execute_cerebras_auth(
    command: CerebrasAuthCommand,
    storage: &crate::credentials::CredentialStorage,
    active_key: Option<(&CerebrasApiKey, CerebrasApiKeySource)>,
) -> Result<String, String> {
    cerebras_auth::execute(command, storage, active_key)
}
