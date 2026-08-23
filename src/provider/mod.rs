mod adapter;
pub mod chatgpt;
mod credentials;
mod openai_auth;
mod speakeasy_auth;

pub use adapter::{
    KitAdapter, KitSession, ModelGroup, ModelSelection, ProviderKind, ReasoningEffort,
    SelectableAdapter, SelectableSession, model_catalog,
};
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
