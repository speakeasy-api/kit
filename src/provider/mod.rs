mod adapter;
pub mod chatgpt;
mod credentials;

pub use adapter::{
    KitAdapter, KitSession, ModelGroup, ModelSelection, ProviderKind, SelectableAdapter,
    SelectableSession, model_catalog,
};
pub use chatgpt::{
    OpenAiSubscriptionAdapter, OpenAiSubscriptionSession, OpenAiSubscriptionTurn,
    SubscriptionConfig,
};
