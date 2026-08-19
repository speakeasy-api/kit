mod adapter;
pub mod chatgpt;
mod credentials;
mod openai_auth;

pub use adapter::{
    KitAdapter, KitSession, ModelGroup, ModelSelection, ProviderKind, SelectableAdapter,
    SelectableSession, model_catalog,
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
pub fn execute_openai_auth(command: OpenAiAuthCommand) -> Result<String, String> {
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
    openai_auth::execute(command, openai_auth::OutputFormat::Human, timeout)
        .map(|output| output.stdout)
        .map_err(|error| error.to_string())
}
